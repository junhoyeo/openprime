//! Bounded, credential-safe diagnostics for kernel bootstrap child commands.

use std::sync::OnceLock;

const STDERR_MAX_CHARS: usize = 2_000;
const STDERR_MAX_LINES: usize = 20;
pub(super) const STDERR_BUFFER_BYTES: usize = 8_000;

fn redaction_patterns() -> &'static [(fancy_regex::Regex, &'static str)] {
    static PATTERNS: OnceLock<Vec<(fancy_regex::Regex, &'static str)>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            (
                r"(?i)\b(Authorization[ \t]*:[ \t]*)(Basic|Bearer|Token)[ \t]+\S+",
                "$1$2 [redacted]",
            ),
            (r"(?i)\b(Bearer|Token)[ \t]+\S+", "$1 [redacted]"),
            (r"(?i)([a-z][a-z0-9+.-]*://)[^\s/@]+@", "$1[redacted]@"),
            (
                r"(?i)([?&](?:[a-z0-9_-]*(?:token|secret|password)|api[_-]?key)=)[^&\s]+",
                "$1[redacted]",
            ),
            (
                r"(?i)\b((?:[a-z0-9_-]*(?:token|secret|password)|api[_-]?key)[ \t]*=[ \t]*)\S+",
                "$1[redacted]",
            ),
            (
                r"(?i)(--?(?:[a-z0-9_-]*(?:token|secret|password)|api[_-]?key)(?:[= \t]+))\S+",
                "$1[redacted]",
            ),
        ]
        .into_iter()
        .map(|(pattern, replacement)| {
            (
                fancy_regex::Regex::new(pattern).expect("bootstrap diagnostic redaction regex"),
                replacement,
            )
        })
        .collect()
    })
}

/// Values carried by credential-shaped environment variables. Literal
/// replacement closes diagnostics where a child prints only the value, with
/// no header/key syntax left for structural redaction.
pub(super) fn ambient_secret_values() -> Vec<String> {
    let mut values = std::env::vars()
        .filter_map(|(name, value)| {
            let name = name.to_ascii_uppercase();
            let credential_shaped = name.ends_with("_TOKEN")
                || name.ends_with("_SECRET")
                || name.ends_with("_PASSWORD")
                || name.ends_with("_CREDENTIAL")
                || name.contains("_API_KEY")
                || name == "AUTHORIZATION";
            (credential_shaped && !value.is_empty()).then_some(value)
        })
        .collect::<Vec<_>>();
    values.sort_by_key(|value| std::cmp::Reverse(value.len()));
    values.dedup();
    values
}

pub(super) fn sanitize_bootstrap_diagnostic(value: &str, secrets: &[String]) -> String {
    let mut sanitized = value.to_string();
    for secret in secrets {
        if !secret.is_empty() {
            sanitized = sanitized.replace(secret, "[redacted]");
        }
    }
    for (pattern, replacement) in redaction_patterns() {
        sanitized = pattern.replace_all(&sanitized, *replacement).into_owned();
    }
    sanitized
}

fn take_last_chars(value: &str, count: usize) -> &str {
    let total = value.chars().count();
    if total <= count {
        return value;
    }
    let start = value
        .char_indices()
        .nth(total - count)
        .map_or(0, |(index, _)| index);
    &value[start..]
}

/// Sanitize complete retained records, then expose only the newest bounded
/// tail. If the byte ring started mid-record, discard that first record before
/// redaction: its credential marker may have fallen outside the ring.
pub(super) fn bounded_stderr_tail(
    value: &str,
    leading_record_truncated: bool,
    secrets: &[String],
) -> String {
    let complete = if leading_record_truncated {
        value.split_once('\n').map_or("", |(_, rest)| rest)
    } else {
        value
    };
    let sanitized = sanitize_bootstrap_diagnostic(complete, secrets);
    let lines = sanitized
        .trim_end_matches(['\r', '\n'])
        .lines()
        .rev()
        .take(STDERR_MAX_LINES)
        .collect::<Vec<_>>();
    let joined = lines.into_iter().rev().collect::<Vec<_>>().join("\n");
    take_last_chars(&joined, STDERR_MAX_CHARS).to_string()
}

#[cfg(test)]
mod tests {
    use super::{bounded_stderr_tail, sanitize_bootstrap_diagnostic};

    #[test]
    fn bootstrap_diagnostic_redacts_credentials_without_crossing_lines() {
        let input = concat!(
            "Authorization: Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==\n",
            "inline Bearer bearer-secret and Token token-secret\n",
            "bare Bearer\nhttps://user:password@example.test/simple\n",
            "client_secret=oauth-client refresh_token=oauth-refresh id_token=oauth-id\n",
            "https://pypi-user:pypi-pass@example.test/simple?access_token=query-secret\n",
            "--api-key cli-api-secret --password=cli-password",
        );
        let sanitized = sanitize_bootstrap_diagnostic(input, &[]);
        for secret in [
            "QWxhZGRpbjpvcGVuIHNlc2FtZQ==",
            "bearer-secret",
            "token-secret",
            "user:password",
            "oauth-client",
            "oauth-refresh",
            "oauth-id",
            "pypi-user",
            "pypi-pass",
            "query-secret",
            "cli-api-secret",
            "cli-password",
        ] {
            assert!(!sanitized.contains(secret), "leaked {secret}: {sanitized}");
        }
        assert!(sanitized.contains("bare Bearer\nhttps://[redacted]@example.test/simple"));
    }

    #[test]
    fn bootstrap_diagnostic_bounds_after_sanitizing_the_retained_tail() {
        let mut input = String::from("Bearer discarded-secret\n");
        for index in 0..40 {
            input.push_str(&format!("diagnostic-{index:02} {}\n", "x".repeat(120)));
        }
        input.push_str("client_secret=tail-secret\nsentinel-newest-line\n");
        let tail = bounded_stderr_tail(&input, false, &[]);
        assert!(tail.contains("sentinel-newest-line"));
        assert!(!tail.contains("discarded-secret"));
        assert!(!tail.contains("tail-secret"));
        assert!(tail.lines().count() <= 20, "{}", tail.lines().count());
        assert!(tail.chars().count() <= 2_000, "{}", tail.chars().count());
    }

    #[test]
    fn bootstrap_diagnostic_discards_a_truncated_leading_record_and_known_secrets() {
        let tail = bounded_stderr_tail(
            "partial-secret-fragment follows\nplain known-value appears",
            true,
            &["known-value".to_string()],
        );
        assert_eq!(tail, "plain [redacted] appears");
    }

    #[test]
    fn bootstrap_diagnostic_never_exposes_an_over_ring_credential_record() {
        let input = format!(
            "{}\nsentinel after long credential",
            "secret-token-fragment".repeat(600)
        );
        let retained = &input[input.len() - 8_000..];
        let tail = bounded_stderr_tail(retained, true, &[]);
        assert_eq!(tail, "sentinel after long credential");
        assert!(!tail.contains("secret-token-fragment"));
    }
}

//! Bounded whole-container privacy validation, before repair or cache shortcuts.
//! Takes an already-open file so the caller retains its ownership/identity guard.
//! This is a structural data check, not an authorization to mutate the path.
use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};

use pa_types::session::privacy::{decode_privacy_row, AdmissionPrivacy, PrivacyRowError};
use sha2::{Digest, Sha256};

/// Bounds are refusal thresholds, never permission to accept a partial scan.
#[derive(Debug, Clone, Copy)]
pub struct PrivacyScanLimits {
    pub max_bytes: u64,
    pub max_row_bytes: usize,
    pub max_rows: usize,
}
impl Default for PrivacyScanLimits {
    fn default() -> Self {
        Self {
            max_bytes: 256 * 1024 * 1024,
            max_row_bytes: 8 * 1024 * 1024,
            max_rows: 1_000_000,
        }
    }
}

/// Validated full-container result. Absent carrier never certifies historical consent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPrivacyScan {
    pub session_id: String,
    pub restriction: Option<AdmissionPrivacy>,
    pub rows: usize,
    pub bytes: u64,
    pub digest: [u8; 32],
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// The reserved carrier's distinguishing field value, and the shortest prefix of it
/// treated as evidence of an attempt. The two failure directions of this threshold
/// are NOT symmetric: a LONGER prefix only widens the blind spot for a real marker
/// truncated partway through this field value (the dangerous, confidentiality-losing
/// direction); a SHORTER prefix only costs more over-refusal of unrelated content
/// that happens to share this substring (the safe direction -- refusing is never the
/// vulnerability this scanner exists to close). This is deliberately short for that
/// reason, not because a collision is "astronomically unlikely" -- a real collision
/// exists ("admission_" appears verbatim in ordinary Kubernetes admission-controller/
/// webhook code, plausibly pasted into a session transcript) and is accepted as the
/// correct cost of closing the more dangerous blind spot.
const MARKER_PREFIX: &[u8] = b"admission_privacy";
const MARKER_MIN_PREFIX_LEN: usize = 6;

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// A NON-SECURITY screen for ORDINARY, accidental corruption (a crash mid-write, bit
/// rot, a torn append) -- not an adversarial-evasion guarantee. A local actor with
/// write access who wants to hide or strip a marker does not need to leave a
/// malformed trace at all: a clean delete or full rewrite defeats any content screen
/// by construction, regardless of this function's threshold. This function only
/// answers: "does this row, which the caller has already confirmed is ambiguously
/// unparseable (not merely rejected for a definite, already-resolved reason), show
/// any recognizable trace of attempting the reserved carrier?" A clean answer here
/// licenses treating the row as ordinary, independent, benign corruption -- the same
/// disposition the existing session loader already applies to unrelated malformed
/// rows -- never as proof of absence of tampering.
///
/// Checks TWO INDEPENDENT signals unconditionally (neither gates the other, and
/// neither is treated as implying the other -- any ordering relationship between a
/// carrier's `customType` key and its value is incidental to one producer's byte
/// layout, not schema-enforced, so it is never relied on as a safety argument here):
/// - any trace of the key name `"customType"` at all, present regardless of its
///   value -- a row carrying this key, however corrupted otherwise, could have been
///   any custom-typed row including this one, so it refuses;
/// - a truncation-robust, short prefix match on the carrier's distinguishing field
///   value itself, independently of the key check above.
///
/// Either signal alone refuses; both absent tolerates.
#[must_use]
fn screens_clean_of_attempted_marker(raw: &[u8]) -> bool {
    if contains_bytes(raw, b"\"customType\"") {
        return false;
    }
    // Checking only the shortest prefix is equivalent to checking every longer one:
    // any occurrence of a longer prefix necessarily contains this one at the same
    // starting position, so there is nothing a longer check could catch that this
    // one would miss.
    !contains_bytes(raw, &MARKER_PREFIX[..MARKER_MIN_PREFIX_LEN])
}

/// Strictly scan the complete opened container and restore the caller's position.
/// No message/row bytes are included in diagnostics. A complete final JSON object
/// may omit its newline; a truncated object or trailing garbage still refuses,
/// UNLESS it is both (a) the physical end of the file (nothing durable follows,
/// matching the append-then-fsync-before-ack contract this format's writer already
/// gives every entry) and (b) free of any trace of an attempted reserved carrier,
/// in which case it is treated as if the file simply ended before it -- an
/// unacknowledged write has no effect, independent of what its partial bytes
/// happened to look like. A malformed row that is NOT the physical end of the file
/// is tolerated only if `screens_clean_of_attempted_marker` passes; otherwise it
/// refuses exactly as before. The header row (first non-blank row) is never
/// eligible for either tolerance: a session must open with a decodable header.
///
/// # Errors
/// Returns an error for malformed/duplicate/unsupported policy that is not covered
/// by the two narrow tolerances above, I/O, bounds, or an observed change to the
/// opened file while scanning.
pub fn scan_opened_container(
    file: &mut File,
    limits: PrivacyScanLimits,
) -> io::Result<SessionPrivacyScan> {
    let position = file.stream_position()?;
    let result = scan(file, limits);
    let restored = file.seek(SeekFrom::Start(position));
    match (result, restored) {
        (Ok(result), Ok(_)) => Ok(result),
        (Err(error), _) | (_, Err(error)) => Err(error),
    }
}

fn scan(file: &mut File, limits: PrivacyScanLimits) -> io::Result<SessionPrivacyScan> {
    let before = file.metadata()?;
    if !before.is_file() {
        return Err(invalid("session privacy input is not a regular file"));
    }
    if before.len() > limits.max_bytes {
        return Err(invalid("session privacy scan exceeds byte limit"));
    }
    file.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(&mut *file);
    let mut line = Vec::new();
    let mut rows = 0;
    let mut bytes = 0_u64;
    let mut session_id = None;
    let mut restriction = None;
    let mut ids = HashSet::new();
    let mut hash = Sha256::new();
    loop {
        line.clear();
        let read = (&mut reader)
            .take(limits.max_row_bytes.saturating_add(1) as u64)
            .read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        bytes = bytes
            .checked_add(read as u64)
            .ok_or_else(|| invalid("session privacy scan byte overflow"))?;
        if read > limits.max_row_bytes || bytes > limits.max_bytes || rows >= limits.max_rows {
            return Err(invalid("session privacy scan exceeds configured limit"));
        }
        hash.update(&line);
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let decoded = decode_privacy_row(&line);
        // MUST stay scoped to the two AMBIGUOUS decode failures only: the row could
        // not even be parsed into a JSON object at all, so decode_privacy_row never
        // reached a verdict about its content. InvalidEnvelope/InvalidPolicy/
        // ReservedCollision mean the row DID parse as an object and was examined and
        // definitively rejected -- that is never ambiguous and must never be screened
        // or tolerated, regardless of position or content, or a row deliberately
        // shaped to fail validation (e.g. a disguised non-"custom"-typed collision
        // row) would read as "a different type entirely" and slip through.
        let ambiguous_parse_failure = matches!(
            decoded,
            Err(PrivacyRowError::InvalidJson | PrivacyRowError::NotObject)
        );
        if decoded.is_err() && rows > 0 && ambiguous_parse_failure {
            // Not the header, and genuinely unparseable (not a resolved rejection):
            // eligible for one of the two narrow tolerances.
            if screens_clean_of_attempted_marker(&line) {
                // POSITIONAL, never shape-based: only a row with nothing durable
                // after it (checked by position, not by how torn it looks) gets the
                // acknowledgment free pass. A row that merely resembles a torn write
                // but sits mid-file, with valid content after it, always falls to
                // the row-independence tolerance below, never the free pass.
                if reader.fill_buf()?.is_empty() {
                    // Torn trailing fragment, no trace of an attempted marker: the
                    // append-then-fsync-before-ack contract means this was never
                    // acknowledged, regardless of its partial content. Stop here;
                    // everything already scanned stands as the complete result.
                    break;
                }
                // An ordinary row's independent, benign corruption: tolerate the
                // same way the existing loader already does, don't count it.
                continue;
            }
            return Err(invalid(
                "session privacy input has malformed or conflicting metadata",
            ));
        }
        let (value, policy) = decoded
            .map_err(|_| invalid("session privacy input has malformed or conflicting metadata"))?;
        if rows == 0 {
            if value.get("type").and_then(serde_json::Value::as_str) != Some("session") {
                return Err(invalid("session privacy input is missing its header"));
            }
            let id = value
                .get("id")
                .and_then(serde_json::Value::as_str)
                .filter(|id| !id.is_empty() && !id.contains('\0'))
                .ok_or_else(|| invalid("session privacy header identity is invalid"))?;
            session_id = Some(id.to_string());
        } else if value.get("type").and_then(serde_json::Value::as_str) == Some("session") {
            return Err(invalid("session privacy input has multiple headers"));
        }
        if let Some(id) = value.get("id").and_then(serde_json::Value::as_str) {
            if !ids.insert(id.to_string()) {
                return Err(invalid(
                    "session privacy input has duplicate entry identities",
                ));
            }
        }
        if policy.is_some() {
            restriction = policy;
        }
        rows += 1;
    }
    drop(reader);
    let after = file.metadata()?;
    if before.len() != after.len()
        || before.modified()? != after.modified()?
        || bytes != after.len()
    {
        return Err(invalid("session privacy input changed during validation"));
    }
    let session_id = session_id.ok_or_else(|| invalid("session privacy input is empty"))?;
    Ok(SessionPrivacyScan {
        session_id,
        restriction,
        rows,
        bytes,
        digest: hash.finalize().into(),
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use pa_types::session::privacy::ADMISSION_PRIVACY_CUSTOM_TYPE;
    use pa_types::session::{EntryBase, FileEntry};
    use serde_json::{json, Map};
    fn rows() -> Vec<serde_json::Value> {
        let marker = AdmissionPrivacy::RestrictedV1
            .entry(EntryBase {
                id: Some("off-branch-marker".into()),
                parent_id: Some("old-leaf".into()),
                timestamp: Some("2026-10-30T00:00:00.000Z".into()),
                rest: Map::default(),
            })
            .unwrap();
        assert!(matches!(marker, FileEntry::Custom { .. }));
        vec![
            json!({"type":"session","version":3,"id":"synthetic-session"}),
            json!({"type":"message","id":"old-leaf","parentId":null}),
            serde_json::to_value(marker).unwrap(),
            json!({"type":"message","id":"selected-leaf","parentId":null}),
        ]
    }
    #[test]
    fn actual_serialized_off_branch_carrier_is_folded_from_entire_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let content = rows()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(&path, &content).unwrap();
        let mut file = File::open(&path).unwrap();
        file.seek(SeekFrom::Start(7)).unwrap();
        let result = scan_opened_container(&mut file, PrivacyScanLimits::default()).unwrap();
        assert_eq!(
            (result.session_id.as_str(), result.restriction, result.rows),
            ("synthetic-session", Some(AdmissionPrivacy::RestrictedV1), 4)
        );
        assert_eq!(file.stream_position().unwrap(), 7);
        assert_eq!(std::fs::read_to_string(path).unwrap(), content);
    }
    #[test]
    fn incomplete_or_capped_input_is_refused_without_repair() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let content = rows()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n{\"customType\":";
        std::fs::write(&path, &content).unwrap();
        let mut file = File::open(&path).unwrap();
        assert!(scan_opened_container(&mut file, PrivacyScanLimits::default()).is_err());
        assert!(scan_opened_container(
            &mut file,
            PrivacyScanLimits {
                max_rows: 1,
                ..PrivacyScanLimits::default()
            }
        )
        .is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), content);
    }

    #[test]
    fn torn_trailing_fragment_with_no_marker_trace_is_tolerated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let header = json!({"type":"session","version":3,"id":"public"}).to_string();
        let message = json!({"type":"message","id":"m1","parentId":null}).to_string();
        let mut content = format!("{header}\n{message}\n").into_bytes();
        // A cut-off UTF-8 continuation byte, matching a write interrupted mid-string.
        content.extend_from_slice(
            b"{\"type\":\"message\",\"id\":\"torn\",\"message\":{\"role\":\"user\",\"con\xc3",
        );
        std::fs::write(&path, &content).unwrap();
        let mut file = File::open(&path).unwrap();
        let result = scan_opened_container(&mut file, PrivacyScanLimits::default()).unwrap();
        assert_eq!(
            (result.session_id.as_str(), result.restriction, result.rows),
            ("public", None, 2)
        );
        assert_eq!(std::fs::read(&path).unwrap(), content);
    }
    #[test]
    fn torn_trailing_fragment_with_a_customtype_trace_still_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let header = json!({"type":"session","version":3,"id":"public"}).to_string();
        let content = format!("{header}\n{{\"customType\":");
        std::fs::write(&path, &content).unwrap();
        let mut file = File::open(&path).unwrap();
        assert!(scan_opened_container(&mut file, PrivacyScanLimits::default()).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
    }
    #[test]
    fn mid_file_malformed_row_with_no_marker_trace_is_tolerated_and_restriction_still_folds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let marker = AdmissionPrivacy::RestrictedV1
            .entry(EntryBase {
                id: Some("privacy-row".into()),
                parent_id: Some("u0".into()),
                timestamp: Some("2026-10-30T00:00:00.000Z".into()),
                rest: Map::default(),
            })
            .unwrap();
        let header = json!({"type":"session","version":3,"id":"mixed"}).to_string();
        let before = json!({"type":"message","id":"u0","parentId":null}).to_string();
        let marker_row = serde_json::to_string(&marker).unwrap();
        let after = json!({"type":"message","id":"u1","parentId":"privacy-row"}).to_string();
        let content = format!("{header}\n{before}\nnot json\n{marker_row}\n{after}\n");
        std::fs::write(&path, &content).unwrap();
        let mut file = File::open(&path).unwrap();
        let result = scan_opened_container(&mut file, PrivacyScanLimits::default()).unwrap();
        assert_eq!(
            (result.session_id.as_str(), result.restriction, result.rows),
            ("mixed", Some(AdmissionPrivacy::RestrictedV1), 4)
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
    }
    #[test]
    fn mid_file_malformed_row_with_a_marker_trace_still_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let header = json!({"type":"session","version":3,"id":"mixed"}).to_string();
        let after = json!({"type":"message","id":"u1","parentId":null}).to_string();
        // Both payloads below are genuinely unparseable (InvalidJson), not merely
        // schema-rejected -- a syntactically valid object with an unrelated
        // customType value (which decode_privacy_row already resolves to Ok(None),
        // never reaching this screen at all) would be the wrong test for this path.
        for suspicious in [
            "{\"customType\":\"admission_privacy_state\", broken}",
            "{\"admission_priv broken}",
        ] {
            let content = format!("{header}\n{suspicious}\n{after}\n");
            std::fs::write(&path, &content).unwrap();
            let mut file = File::open(&path).unwrap();
            assert!(scan_opened_container(&mut file, PrivacyScanLimits::default()).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
        }
    }
    #[test]
    fn a_syntactically_valid_row_with_an_unrelated_customtype_never_reaches_the_screen() {
        // decode_privacy_row resolves this definitively (Ok, no restriction) --
        // confirms the screen path is never even entered for a row that parses.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let header = json!({"type":"session","version":3,"id":"mixed"}).to_string();
        let ordinary = json!({"customType":"something_else"}).to_string();
        let after = json!({"type":"message","id":"u1","parentId":null}).to_string();
        let content = format!("{header}\n{ordinary}\n{after}\n");
        std::fs::write(&path, &content).unwrap();
        let mut file = File::open(&path).unwrap();
        let result = scan_opened_container(&mut file, PrivacyScanLimits::default()).unwrap();
        assert_eq!(
            (result.session_id.as_str(), result.restriction, result.rows),
            ("mixed", None, 3)
        );
    }
    #[test]
    fn a_byte_screen_clean_reserved_collision_row_still_refuses() {
        // The MUST-FIX 1 regression test: a row that is BOTH (a) a genuine,
        // definitively-resolved ReservedCollision once parsed, AND (b) clean under
        // screens_clean_of_attempted_marker's raw-byte scan, because one character
        // each in the "customType" key and the "admission_privacy_state" value is
        // JSON \u-escaped. serde_json resolves \u0063 to the literal byte 'c' during
        // parsing, so decode_privacy_row still sees the real strings and reaches
        // ReservedCollision -- but the RAW, UNPARSED bytes contain neither the
        // literal "customType" substring nor an "admission_" prefix, so the screen
        // alone would wrongly call this clean. Only the MUST-FIX 1 scoping (never
        // screening a row decode_privacy_row already definitively resolved) refuses
        // this row; a version that screened every decode error unconditionally would
        // tolerate it and reopen the collision bypass. A prior version of this test
        // used a plain, unescaped disguised row, which the screen alone already
        // catches via the literal "customType" substring -- that version proved
        // nothing about the MUST-FIX 1 boundary specifically; this one does.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let header = json!({"type":"session","version":3,"id":"mixed"}).to_string();
        let disguised = concat!(
            r#"{"type":"message","message":{"role":"custom","#,
            r#""\u0063ustomType":"\u0061dmission_privacy_state","content":"synthetic"}}"#,
        );
        assert!(
            !disguised.contains("\"customType\""),
            "fixture must not contain the literal key substring, or it proves nothing"
        );
        assert!(!contains_bytes(disguised.as_bytes(),&MARKER_PREFIX[..MARKER_MIN_PREFIX_LEN]),
            "fixture must not contain the literal value prefix the production threshold actually checks, or it proves nothing");
        let parsed: serde_json::Value = serde_json::from_str(disguised).unwrap();
        assert_eq!(
            parsed["message"]["customType"], ADMISSION_PRIVACY_CUSTOM_TYPE,
            "fixture must still resolve to the real reserved name once parsed"
        );
        let after = json!({"type":"message","id":"u1","parentId":null}).to_string();
        let content = format!("{header}\n{disguised}\n{after}\n");
        std::fs::write(&path, &content).unwrap();
        let mut file = File::open(&path).unwrap();
        assert!(scan_opened_container(&mut file, PrivacyScanLimits::default()).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
    }
    #[test]
    fn header_row_itself_is_never_eligible_for_either_tolerance() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let content = "not json at all".to_string();
        std::fs::write(&path, &content).unwrap();
        let mut file = File::open(&path).unwrap();
        assert!(scan_opened_container(&mut file, PrivacyScanLimits::default()).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
    }

    #[test]
    fn complete_final_object_needs_no_newline_but_partial_or_trailing_bytes_refuse() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no-newline.jsonl");
        let public = serde_json::json!({"type":"session","version":3,"id":"public"}).to_string();
        std::fs::write(&path, &public).unwrap();
        let mut file = File::open(&path).unwrap();
        let scan = scan_opened_container(&mut file, PrivacyScanLimits::default()).unwrap();
        assert_eq!(
            (scan.session_id.as_str(), scan.restriction),
            ("public", None)
        );
        for suffix in [
            " trailing",
            "\n{\"customType\":",
            "\n{\"type\":\"custom\",\"customType\":\"admission_privacy_state\"",
        ] {
            let invalid = public.clone() + suffix;
            std::fs::write(&path, &invalid).unwrap();
            let mut file = File::open(&path).unwrap();
            assert!(scan_opened_container(&mut file, PrivacyScanLimits::default()).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), invalid);
        }
    }
}

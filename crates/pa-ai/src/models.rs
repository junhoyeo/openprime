//! Model helpers: cost calculation over the shared model types.

use pa_types::JsNumber;

pub use pa_types::ai::thinking_levels::{
    clamp_thinking_level, get_supported_thinking_levels, models_are_equal, supports_thinking,
    thinking_level_from_str, thinking_level_map, EXTENDED_THINKING_LEVELS,
    SUPPORTED_THINKING_LEVELS,
};

use crate::types::{Model, Usage, UsageCost};

/// `OpenCode`'s public credential for free Zen models. These models remain out of automatic model
/// discovery unless the provider has configured auth; the fallback applies only after a user
/// explicitly selects one.
pub const OPENCODE_PUBLIC_API_KEY: &str = "public";

// Reviewed 2026-10-26 against prime-agent-catalog `models/catalog.v1.json` commit
// 01f4d46dd5157ec73f8faf440214088237c81d59 (2026-10-23), raw-content SHA-256
// f057223709acfc535ffc0395766889208e5104423ce3612ad10c242a2d7bec9d. The bundled fixture's
// seven-entry free set matches that source; `pa-models/tests/parity.rs` pins the drift check.
// This is a trust allowlist, not a name heuristic: a newly cataloged free model stays fail-closed
// until its official Zen route and all four prices are reviewed and this list is updated alongside
// the pa-models fixture drift test.
const OPENCODE_PUBLIC_MODEL_IDS: &[&str] = &[
    "big-pickle",
    "ling-3.0-flash-fin-free",
    "mimo-v2.5-free",
    "muse-spark-1.2-contributor-free",
    "muse-spark-1.3-contributor-free",
    "nemotron-3-ultra-free",
    "nemotron-3.5-lightning-free",
];

/// Whether `model` is a reviewed zero-cost model on the canonical `OpenCode` Zen route.
#[must_use]
pub fn is_opencode_public_model(model: &Model) -> bool {
    let trusted_catalog_entry = model.provider == "opencode"
        && model.base_url == "https://opencode.ai/zen/v1"
        && OPENCODE_PUBLIC_MODEL_IDS.contains(&model.id.as_str());
    trusted_catalog_entry
        && [
            model.cost.input.as_f64(),
            model.cost.output.as_f64(),
            model.cost.cache_read.as_f64(),
            model.cost.cache_write.as_f64(),
        ]
        .into_iter()
        .all(|cost| cost == 0.0)
}

/// Prefer a configured key, otherwise use `OpenCode`'s public credential for a free Zen model.
#[must_use]
pub fn opencode_public_api_key(model: &Model, configured: Option<String>) -> Option<String> {
    configured
        .filter(|key| !key.is_empty())
        .or_else(|| is_opencode_public_model(model).then(|| OPENCODE_PUBLIC_API_KEY.into()))
}

#[derive(Debug, Clone, Default)]
pub struct CostOverrides {
    pub cache_write: Option<f64>,
}

/// Compute and write the cost breakdown in place on `usage.cost`.
pub fn calculate_cost(model: &Model, usage: &mut Usage, overrides: Option<&CostOverrides>) {
    usage.cost = calculate_cost_values(model, usage, overrides);
}

#[must_use]
pub fn calculate_cost_values(
    model: &Model,
    usage: &Usage,
    overrides: Option<&CostOverrides>,
) -> UsageCost {
    let cache_write_cost = overrides
        .and_then(|overrides| overrides.cache_write)
        .unwrap_or_else(|| model.cost.cache_write.as_f64());
    // Token counts sit far below f64's 2^53 exact-integer range; the cost math is f64 by design.
    #[allow(clippy::cast_precision_loss)]
    let input = (model.cost.input.as_f64() / 1_000_000.0) * usage.input as f64;
    #[allow(clippy::cast_precision_loss)]
    let output = (model.cost.output.as_f64() / 1_000_000.0) * usage.output as f64;
    #[allow(clippy::cast_precision_loss)]
    let cache_read = (model.cost.cache_read.as_f64() / 1_000_000.0) * usage.cache_read as f64;
    #[allow(clippy::cast_precision_loss)]
    let cache_write = (cache_write_cost / 1_000_000.0) * usage.cache_write as f64;
    UsageCost {
        input: JsNumber::from(input),
        output: JsNumber::from(output),
        cache_read: JsNumber::from(cache_read),
        cache_write: JsNumber::from(cache_write),
        total: JsNumber::from(input + output + cache_read + cache_write),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ModelCost, ModelInput, ModelThinkingLevel};
    use pa_types::JsNumber;

    fn model(
        reasoning: bool,
        map: Option<std::collections::BTreeMap<ModelThinkingLevel, Option<String>>>,
    ) -> Model {
        Model {
            id: "m".into(),
            name: "m".into(),
            api: "openai-completions".into(),
            provider: "test".into(),
            base_url: "http://localhost".into(),
            reasoning,
            thinking_level_map: map,
            input: vec![ModelInput::Text],
            cost: ModelCost {
                input: JsNumber::from(0.0),
                output: JsNumber::from(0.0),
                cache_read: JsNumber::from(0.0),
                cache_write: JsNumber::from(0.0),
            },
            context_window: 128_000,
            max_tokens: 8192,
            max_tokens_explicit: false,
            featured: None,
            headers: None,
            compat: None,
        }
    }

    #[test]
    fn public_opencode_key_applies_only_to_zero_cost_zen_models() {
        let free_zen = Model {
            id: "big-pickle".into(),
            provider: "opencode".into(),
            base_url: "https://opencode.ai/zen/v1".into(),
            ..model(false, None)
        };
        assert!(is_opencode_public_model(&free_zen));
        assert_eq!(
            opencode_public_api_key(&free_zen, None).as_deref(),
            Some(OPENCODE_PUBLIC_API_KEY)
        );
        assert_eq!(
            opencode_public_api_key(&free_zen, Some("configured".into())).as_deref(),
            Some("configured")
        );
        assert_eq!(
            opencode_public_api_key(&free_zen, Some(String::new())).as_deref(),
            Some(OPENCODE_PUBLIC_API_KEY)
        );

        for cost in [
            ModelCost {
                input: JsNumber::from(1.0),
                ..free_zen.cost
            },
            ModelCost {
                output: JsNumber::from(1.0),
                ..free_zen.cost
            },
            ModelCost {
                cache_read: JsNumber::from(1.0),
                ..free_zen.cost
            },
            ModelCost {
                cache_write: JsNumber::from(1.0),
                ..free_zen.cost
            },
            ModelCost {
                output: JsNumber::from(-1.0),
                ..free_zen.cost
            },
            ModelCost {
                cache_read: JsNumber::from(f64::NAN),
                ..free_zen.cost
            },
            ModelCost {
                cache_write: JsNumber::from(f64::INFINITY),
                ..free_zen.cost
            },
        ] {
            let paid_zen = Model {
                cost,
                ..free_zen.clone()
            };
            assert!(!is_opencode_public_model(&paid_zen));
            assert_eq!(opencode_public_api_key(&paid_zen, None), None);
        }

        let unknown_official_model = Model {
            id: "owner-declared-free".into(),
            ..free_zen.clone()
        };
        assert!(!is_opencode_public_model(&unknown_official_model));
        assert_eq!(opencode_public_api_key(&unknown_official_model, None), None);

        for base_url in [
            "http://127.0.0.1:8787/v1",
            "https://user@opencode.ai/zen/v1",
            "https://opencode.ai:443/zen/v1",
            "https://opencode.ai/zen/v1/extra",
            "https://opencode.ai/zen/v1?route=other",
            "https://opencode.ai/zen/v1#fragment",
            "https://opencode.example/zen/v1",
            "https://opencode.ai/zen/v1/",
        ] {
            let untrusted_destination = Model {
                base_url: base_url.into(),
                ..free_zen.clone()
            };
            assert!(!is_opencode_public_model(&untrusted_destination));
            assert_eq!(
                opencode_public_api_key(&untrusted_destination, None),
                None,
                "{base_url}"
            );
        }

        let unrelated_free = Model {
            provider: "gateway".into(),
            ..free_zen
        };
        assert!(!is_opencode_public_model(&unrelated_free));
        assert_eq!(opencode_public_api_key(&unrelated_free, None), None);
    }

    #[test]
    fn computes_cost() {
        let model = Model {
            cost: ModelCost {
                input: JsNumber::from(10.0),
                output: JsNumber::from(50.0),
                cache_read: JsNumber::from(1.0),
                cache_write: JsNumber::from(12.5),
            },
            ..model(false, None)
        };
        let mut usage = Usage {
            input: 1_000_000,
            output: 100_000,
            cache_read: 10_000,
            cache_write: 20_000,
            ..Usage::default()
        };
        calculate_cost(&model, &mut usage, None);
        assert!((usage.cost.input.as_f64() - 10.0).abs() < 1e-9);
        assert!((usage.cost.output.as_f64() - 5.0).abs() < 1e-9);
        assert!((usage.cost.cache_read.as_f64() - 0.01).abs() < 1e-9);
        assert!((usage.cost.cache_write.as_f64() - 0.25).abs() < 1e-9);
        assert!((usage.cost.total.as_f64() - (10.0 + 5.0 + 0.01 + 0.25)).abs() < 1e-9);
    }

    #[test]
    fn thinking_helpers_stay_re_exported() {
        let m = model(false, None);
        assert_eq!(
            get_supported_thinking_levels(&m),
            vec![pa_types::ai::ModelThinkingLevel::Off]
        );
        assert!(models_are_equal(Some(&m), Some(&m)));
    }
}

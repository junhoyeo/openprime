//! Model helpers: cost calculation over the shared model types.

use pa_types::JsNumber;

pub use pa_types::ai::thinking_levels::{
    clamp_thinking_level, get_supported_thinking_levels, models_are_equal, supports_thinking,
    thinking_level_from_str, thinking_level_map, EXTENDED_THINKING_LEVELS,
    SUPPORTED_THINKING_LEVELS,
};

use crate::types::{Model, Usage, UsageCost};

/// OpenCode's public credential for free Zen models. These models remain out of automatic model
/// discovery unless the provider has configured auth; the fallback applies only after a user
/// explicitly selects one.
pub const OPENCODE_PUBLIC_API_KEY: &str = "public";

/// Whether `model` is a zero-cost model on an OpenCode Zen provider.
#[must_use]
pub fn is_opencode_public_model(model: &Model) -> bool {
    matches!(model.provider.as_str(), "opencode" | "opencode-go")
        && model.cost.input.as_f64() == 0.0
}

/// Prefer a configured key, otherwise use OpenCode's public credential for a free Zen model.
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
            provider: "opencode".into(),
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

        let paid_zen = Model {
            cost: ModelCost {
                input: JsNumber::from(1.0),
                ..free_zen.cost.clone()
            },
            ..free_zen.clone()
        };
        assert!(!is_opencode_public_model(&paid_zen));
        assert_eq!(opencode_public_api_key(&paid_zen, None), None);

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

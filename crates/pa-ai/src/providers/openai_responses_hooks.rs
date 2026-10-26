//! `OpenAI` Responses stream hooks, reasoning-summary options, and service-tier pricing.

use crate::types::Usage;

/// Reasoning summary mode requested from the Responses API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // full TS option surface; variants set by callers
pub enum ReasoningSummary {
    Auto,
    Detailed,
    Concise,
}

impl ReasoningSummary {
    pub fn as_str(self) -> &'static str {
        match self {
            ReasoningSummary::Auto => "auto",
            ReasoningSummary::Detailed => "detailed",
            ReasoningSummary::Concise => "concise",
        }
    }
}

/// Hook resolving the effective service tier from response and request tiers.
pub type ResolveServiceTierFn =
    Box<dyn Fn(Option<String>, Option<String>) -> Option<String> + Send>;
/// Hook applying service-tier pricing to a usage block.
pub type ApplyServiceTierPricingFn = Box<dyn Fn(&mut Usage, Option<String>) + Send>;

/// Service-tier hooks mirroring the TS `OpenAIResponsesStreamOptions`.
#[derive(Default)]
pub struct ResponsesStreamHooks {
    pub request_service_tier: Option<crate::types::ServiceTier>,
    pub resolve_service_tier: Option<ResolveServiceTierFn>,
    pub apply_service_tier_pricing: Option<ApplyServiceTierPricingFn>,
}

/// Resolve the regular Responses API tier: an explicit response value is authoritative; only an
/// omitted response tier falls back to what the request asked for.
pub(crate) fn resolve_standard_service_tier(
    response_tier: Option<String>,
    request_tier: Option<String>,
) -> Option<String> {
    response_tier.or(request_tier)
}

/// Multipliers per <https://developers.openai.com/api/docs/pricing>.
pub fn get_service_tier_cost_multiplier(model_id: &str, service_tier: Option<&str>) -> f64 {
    match service_tier {
        Some("flex") => 0.5,
        Some("priority") => {
            if model_id == "gpt-5.5" {
                2.5
            } else {
                2.0
            }
        }
        _ => 1.0,
    }
}

/// Apply service-tier pricing multipliers to a usage block.
pub fn apply_service_tier_pricing(usage: &mut Usage, service_tier: Option<&str>, model_id: &str) {
    let multiplier = get_service_tier_cost_multiplier(model_id, service_tier);
    // The multiplier table is discrete; equality with the 1.0 sentinel is the no-op contract.
    #[allow(clippy::float_cmp)]
    if multiplier == 1.0 {
        return;
    }
    usage.cost.input = (usage.cost.input.as_f64() * multiplier).into();
    usage.cost.output = (usage.cost.output.as_f64() * multiplier).into();
    usage.cost.cache_read = (usage.cost.cache_read.as_f64() * multiplier).into();
    usage.cost.cache_write = (usage.cost.cache_write.as_f64() * multiplier).into();
    usage.cost.total = (usage.cost.input.as_f64()
        + usage.cost.output.as_f64()
        + usage.cost.cache_read.as_f64()
        + usage.cost.cache_write.as_f64())
    .into();
}

#[cfg(test)]
mod tests {
    use super::resolve_standard_service_tier;

    #[test]
    fn response_tier_wins_and_an_omission_falls_back_to_the_request() {
        assert_eq!(
            resolve_standard_service_tier(Some("default".into()), Some("priority".into())),
            Some("default".into())
        );
        assert_eq!(
            resolve_standard_service_tier(None, Some("priority".into())),
            Some("priority".into())
        );
        assert_eq!(resolve_standard_service_tier(None, None), None);
    }
}

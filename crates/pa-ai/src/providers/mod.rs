//! Streaming provider implementations ported from `packages/ai/src/providers`.

pub mod anthropic;
pub mod azure_openai_responses;
pub mod bedrock;
pub mod faux;
pub mod google;
pub mod google_shared;
pub mod google_stream;
pub mod google_vertex;
pub mod mistral;
pub mod openai_codex_responses;
pub mod openai_completions;
pub mod openai_responses;
pub mod openai_responses_hooks;
pub mod openai_responses_shared;
pub mod openai_responses_stream;
mod opencode_headers;
pub mod simple_options;
pub mod transform_messages;

#[cfg(test)]
mod public_zen_request_tests {
    use super::*;
    use crate::types::{Model, ModelCost, ModelInput};

    fn model(api: &str, trusted: bool) -> Model {
        Model {
            id: if trusted { "big-pickle" } else { "owner-model" }.into(),
            name: "model".into(),
            api: api.into(),
            provider: "opencode".into(),
            base_url: if trusted {
                "https://opencode.ai/zen/v1"
            } else {
                "http://127.0.0.1:8787/v1"
            }
            .into(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            cost: if trusted {
                ModelCost {
                    input: 0.0.into(),
                    output: 0.0.into(),
                    cache_read: 0.0.into(),
                    cache_write: 0.0.into(),
                }
            } else {
                ModelCost {
                    input: 1.0.into(),
                    output: 2.0.into(),
                    cache_read: 0.1.into(),
                    cache_write: 0.5.into(),
                }
            },
            context_window: 128_000,
            max_tokens: 8192,
            max_tokens_explicit: false,
            featured: None,
            headers: None,
            compat: None,
        }
    }

    fn resolved_headers(
        api: &str,
        model: &Model,
        configured: Option<String>,
    ) -> Option<Vec<(String, String)>> {
        match api {
            "anthropic-messages" => anthropic::build_resolved_request_headers(
                model,
                configured,
                false,
                false,
                None,
                Some("conversation"),
            )
            .map(|(headers, _)| headers),
            "openai-completions" => {
                let compat = openai_completions::get_compat(model);
                openai_completions::build_resolved_headers(
                    model,
                    configured,
                    None,
                    None,
                    &compat,
                    Some("conversation"),
                )
            }
            "openai-responses" => {
                let options = openai_responses::OpenAIResponsesOptions::default();
                openai_responses::build_resolved_headers(model, configured, &options, None)
            }
            "google-generative-ai" => {
                google::build_resolved_headers(model, configured, None, Some("conversation"))
            }
            other => panic!("unexpected transport {other}"),
        }
    }

    fn auth_header(api: &str, key: &str) -> (String, String) {
        match api {
            "anthropic-messages" => ("x-api-key".into(), key.into()),
            "google-generative-ai" => ("x-goog-api-key".into(), key.into()),
            "openai-completions" | "openai-responses" => {
                ("Authorization".into(), format!("Bearer {key}"))
            }
            other => panic!("unexpected transport {other}"),
        }
    }

    #[test]
    fn reviewed_public_fallback_and_configured_precedence_reach_every_request_builder() {
        for api in [
            "anthropic-messages",
            "openai-completions",
            "openai-responses",
            "google-generative-ai",
        ] {
            let trusted = model(api, true);
            let public = resolved_headers(api, &trusted, None).expect("reviewed public route");
            assert!(public.contains(&auth_header(api, crate::models::OPENCODE_PUBLIC_API_KEY)));

            let configured = resolved_headers(api, &trusted, Some("configured-key".into()))
                .expect("configured key");
            assert!(configured.contains(&auth_header(api, "configured-key")));
            assert!(
                !configured.contains(&auth_header(api, crate::models::OPENCODE_PUBLIC_API_KEY,))
            );

            let untrusted = model(api, false);
            assert_eq!(resolved_headers(api, &untrusted, None), None, "{api}");
            let custom = resolved_headers(api, &untrusted, Some("custom-key".into()))
                .expect("explicit custom credential");
            assert!(custom.contains(&auth_header(api, "custom-key")));
        }
    }
}

use pa_ai::types::{CacheRetention, SimpleStreamOptions, StreamOptions};
use pa_types::ai::{Context, Model};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn capture(
    api: &str,
    provider: &str,
    id: &str,
    api_key: Option<&str>,
    headers: Option<std::collections::HashMap<String, String>>,
) -> Vec<(String, String)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buf = [0; 4096];
        while !request.windows(4).any(|b| b == b"\r\n\r\n") {
            let count = socket.read(&mut buf).await.unwrap();
            assert!(count > 0);
            request.extend_from_slice(&buf[..count]);
        }
        socket
            .write_all(
                b"HTTP/1.1 400 Bad Request\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
            )
            .await
            .unwrap();
        String::from_utf8(request)
            .unwrap()
            .split("\r\n\r\n")
            .next()
            .unwrap()
            .lines()
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_string()))
            .collect::<Vec<_>>()
    });
    let model: Model = serde_json::from_value(json!({
        "id":"test-model", "name":"Test", "provider":provider, "api":api,
        "baseUrl":format!("http://{address}"), "reasoning":false, "input":["text"],
        "contextWindow":10000, "maxTokens":100,
        "cost":{"input":1,"output":2,"cacheRead":0.1,"cacheWrite":0.5}
    }))
    .unwrap();
    let options = SimpleStreamOptions::from_base(StreamOptions {
        api_key: api_key.map(str::to_string),
        session_id: Some(id.into()),
        cache_retention: Some(CacheRetention::None),
        timeout_ms: Some(5_000),
        headers,
        ..Default::default()
    });
    let context = Context {
        system_prompt: None,
        messages: vec![],
        tools: None,
    };
    let _ = pa_ai::complete_simple(&model, &context, Some(options))
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn opencode_session_identity_reaches_every_transport_without_cache_retention() {
    for api in [
        "openai-completions",
        "openai-responses",
        "anthropic-messages",
        "google-generative-ai",
    ] {
        let headers = capture(
            api,
            "opencode-go",
            "conversation",
            Some("synthetic-only"),
            None,
        )
        .await;
        assert_eq!(
            headers
                .iter()
                .filter(|(name, _)| name == "x-opencode-session")
                .map(|(_, value)| value.as_str())
                .collect::<Vec<_>>(),
            vec!["conversation"],
            "{api}"
        );
        assert!(
            headers.contains(&("user-agent".into(), "prime-agent".into())),
            "{api}: {headers:?}"
        );
        let configured_auth = match api {
            "anthropic-messages" => ("x-api-key", "synthetic-only".to_string()),
            "google-generative-ai" => ("x-goog-api-key", "synthetic-only".to_string()),
            "openai-completions" | "openai-responses" => {
                ("authorization", "Bearer synthetic-only".to_string())
            }
            other => panic!("unexpected transport {other}"),
        };
        assert!(
            headers.contains(&(configured_auth.0.into(), configured_auth.1)),
            "configured key missing on {api}: {headers:?}"
        );
    }
}

#[tokio::test]
async fn opencode_session_preserves_case_insensitive_explicit_overrides() {
    for api in [
        "openai-completions",
        "openai-responses",
        "anthropic-messages",
        "google-generative-ai",
    ] {
        let explicit = std::collections::HashMap::from([
            ("X-OpenCode-Session".into(), "explicit".into()),
            ("User-Agent".into(), "custom-client".into()),
        ]);
        let headers = capture(
            api,
            "opencode-go",
            "conversation",
            Some("synthetic-only"),
            Some(explicit),
        )
        .await;
        assert_eq!(
            headers
                .iter()
                .filter(|(name, _)| name == "x-opencode-session")
                .map(|(_, value)| value.as_str())
                .collect::<Vec<_>>(),
            vec!["explicit"]
        );
        assert!(headers.contains(&("user-agent".into(), "custom-client".into())));
    }
}

#[tokio::test]
async fn an_untrusted_opencode_endpoint_never_receives_the_public_literal() {
    for api in [
        "openai-completions",
        "openai-responses",
        "anthropic-messages",
        "google-generative-ai",
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let model: Model = serde_json::from_value(json!({
            "id":"untrusted-free", "name":"Untrusted", "provider":"opencode", "api":api,
            "baseUrl":format!("http://{address}"), "reasoning":false, "input":["text"],
            "contextWindow":10000, "maxTokens":100,
            "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0}
        }))
        .unwrap();
        let context = Context {
            system_prompt: None,
            messages: vec![],
            tools: None,
        };
        let message = match tokio::time::timeout(
            std::time::Duration::from_secs(2),
            pa_ai::complete_simple(&model, &context, None),
        )
        .await
        {
            Ok(Ok(message)) => message,
            Ok(Err(error)) => panic!("{api} failed before request inspection: {error}"),
            Err(error) => panic!("{api} attempted the untrusted endpoint: {error}"),
        };
        assert_eq!(
            message.stop_reason,
            pa_ai::types::StopReason::Error,
            "{api}"
        );
        let listener = listener.into_std().unwrap();
        listener.set_nonblocking(true).unwrap();
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
            "{api} connected to the untrusted endpoint"
        );
    }
}

#[tokio::test]
async fn unrelated_gateway_does_not_receive_opencode_identity() {
    let headers = capture(
        "openai-completions",
        "custom-gateway",
        "conversation",
        Some("synthetic-only"),
        None,
    )
    .await;
    assert!(headers.iter().all(|(name, _)| name != "x-opencode-session"));
}

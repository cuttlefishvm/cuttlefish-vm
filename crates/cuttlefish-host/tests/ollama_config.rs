//! Configured Ollama uses only a loopback fixture; no model is needed.
use cuttlefish_host::infer::{InferBackend, InferRequest};
use cuttlefish_host::ollama::OllamaBackend;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn target(extra: Value) -> String {
    let mut config = json!({"model": "test:1", "expected_digest": DIGEST});
    config
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    config.to_string()
}
fn tags(digest: &str) -> String {
    json!({"models": [{"name": "test:1", "model": "test:1", "digest": digest}]}).to_string()
}
fn generated(model: &str) -> String {
    format!(
        "{}\n",
        json!({"model": model, "response": "{}", "done": true, "prompt_eval_count": 4, "eval_count": 2})
    )
}

async fn server(responses: Vec<String>) -> (String, tokio::task::JoinHandle<Vec<(String, Value)>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for response in responses {
            let (mut stream, _) =
                tokio::time::timeout(std::time::Duration::from_secs(5), listener.accept())
                    .await
                    .unwrap()
                    .unwrap();
            let mut bytes = Vec::new();
            let (header_end, length) = loop {
                let mut chunk = [0; 4096];
                let n = stream.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&chunk[..n]);
                if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .map(|v| v.parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    break (end + 4, length);
                }
            };
            while bytes.len() < header_end + length {
                let mut chunk = [0; 4096];
                let n = stream.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&chunk[..n]);
            }
            let request = String::from_utf8_lossy(&bytes[..header_end])
                .lines()
                .next()
                .unwrap()
                .to_string();
            let body = if length == 0 {
                Value::Null
            } else {
                serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap()
            };
            requests.push((request, body));
            let wire = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}", response.len(), response);
            stream.write_all(wire.as_bytes()).await.unwrap();
        }
        requests
    });
    (host, task)
}

#[test]
fn rejects_invalid_configuration_before_any_network_access() {
    for config in [
        json!({"model":"test:1"}),
        json!({"model":"test:1", "expected_digest":"short"}),
        json!({"model":"", "expected_digest":DIGEST}),
        json!({"model":"test", "expected_digest":DIGEST}),
        serde_json::from_str(&target(json!({"unknown":1}))).unwrap(),
        serde_json::from_str(&target(json!({"temperature":-0.1}))).unwrap(),
        serde_json::from_str(&target(json!({"think":"maybe"}))).unwrap(),
        serde_json::from_str(&target(json!({"seed":1.5}))).unwrap(),
        serde_json::from_str(&target(json!({"format":"xml"}))).unwrap(),
        serde_json::from_str(&target(json!({"format":{"type":"bogus"}}))).unwrap(),
    ] {
        assert!(
            OllamaBackend::from_target("http://unused", &config.to_string()).is_err(),
            "accepted {config}"
        );
    }
}

#[tokio::test]
async fn sends_explicit_controls_and_checks_digest_on_both_sides() {
    let (host, server) = server(vec![tags(DIGEST), generated("test:1"), tags(DIGEST)]).await;
    let backend = OllamaBackend::from_target(
        host,
        &target(json!({"seed":42,"temperature":0.0,"format":"json","think":false})),
    )
    .unwrap();
    assert_eq!(backend.model_name(), format!("test:1@sha256:{DIGEST}"));
    let result = backend
        .infer(InferRequest::new("hello", 100), &mut |_| true)
        .await
        .unwrap();
    assert_eq!(result.text, "{}");
    assert_eq!((result.tokens_in, result.tokens_out), (4, 2));
    let requests = server.await.unwrap();
    assert_eq!(
        requests.iter().map(|(r, _)| r.as_str()).collect::<Vec<_>>(),
        [
            "GET /api/tags HTTP/1.1",
            "POST /api/generate HTTP/1.1",
            "GET /api/tags HTTP/1.1"
        ]
    );
    assert_eq!(
        requests[1].1,
        json!({"model":"test:1", "prompt":"hello", "stream":true, "images":[], "options":{"num_predict":100,"seed":42,"temperature":0.0},"format":"json","think":false})
    );
}

#[tokio::test]
async fn mismatched_pin_stops_before_generation() {
    let (host, server) = server(vec![tags(&"b".repeat(64))]).await;
    let backend = OllamaBackend::from_target(host, &target(json!({}))).unwrap();
    let error = backend
        .infer(InferRequest::new("hello", 100), &mut |_| true)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("digest mismatch"), "{error}");
    assert_eq!(server.await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_tag_changed_during_generation_fails_the_result() {
    let (host, server) = server(vec![
        tags(DIGEST),
        generated("test:1"),
        tags(&"b".repeat(64)),
    ])
    .await;
    let backend = OllamaBackend::from_target(host, &target(json!({}))).unwrap();
    let error = backend
        .infer(InferRequest::new("hello", 100), &mut |_| true)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("digest mismatch"), "{error}");
    assert_eq!(server.await.unwrap().len(), 3);
}

#[tokio::test]
async fn response_model_must_match_the_pinned_tag() {
    let (host, server) = server(vec![tags(DIGEST), generated("another:1")]).await;
    let backend = OllamaBackend::from_target(host, &target(json!({}))).unwrap();
    let error = backend
        .infer(InferRequest::new("hello", 100), &mut |_| true)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("response model"), "{error}");
    assert_eq!(server.await.unwrap().len(), 2);
}

#[tokio::test]
async fn schema_is_forwarded_and_truncated_stream_is_not_success() {
    let (host, server) = server(vec![
        tags(DIGEST),
        format!("{}\n", json!({"model":"test:1","response":"partial"})),
    ])
    .await;
    let schema =
        json!({"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"]});
    let backend = OllamaBackend::from_target(host, &target(json!({"format":schema}))).unwrap();
    let error = backend
        .infer(InferRequest::new("hello", 100), &mut |_| true)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("terminal"), "{error}");
    assert_eq!(server.await.unwrap()[1].1["format"], schema);
}

#[tokio::test]
async fn missing_or_ambiguous_tag_identity_fails() {
    for response in [
        json!({"models":[]}),
        json!({"models":[{"name":"test:1","digest":DIGEST},{"name":"test:1","digest":DIGEST}]}),
        json!({"models":[{"name":"test:1"}]}),
    ] {
        let (host, server) = server(vec![response.to_string()]).await;
        let backend = OllamaBackend::from_target(host, &target(json!({}))).unwrap();
        assert!(backend
            .infer(InferRequest::new("hello", 100), &mut |_| true)
            .await
            .is_err());
        assert_eq!(server.await.unwrap().len(), 1);
    }
}

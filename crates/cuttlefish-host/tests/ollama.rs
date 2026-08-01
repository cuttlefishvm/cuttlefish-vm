//! Tests for the Ollama backend.
//!
//! These run against a mock server rather than a real Ollama, so CI needs
//! nothing installed and the assertions are deterministic. The mock speaks
//! genuine HTTP/1.1 chunked encoding and splits JSON lines *across* chunk
//! boundaries on purpose — a client that assumes one chunk is one line passes a
//! naive fake and fails against the real thing.
//!
//! One test does talk to a real Ollama. It is `#[ignore]`d, so it runs only when
//! asked:
//!
//! ```console
//! $ cargo test -p cuttlefish-host --test ollama -- --ignored
//! ```

use cuttlefish_host::{infer::InferBackend, ollama::OllamaBackend};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Serve one HTTP request, writing `body` as chunked transfer-encoding split
/// into `pieces` byte-sized chunks. Returns the bound address.
///
/// Deliberately raw rather than an HTTP-server dependency: the point is to
/// control exactly where chunk boundaries land, which a higher-level server
/// hides.
async fn mock_ollama(body: &'static str, pieces: usize, status: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();

        // Read the request headers so the client's write completes; the body is
        // irrelevant to what is being tested.
        let mut buf = [0u8; 4096];
        let _ = sock.read(&mut buf).await;

        sock.write_all(
            format!("HTTP/1.1 {status}\r\nContent-Type: application/x-ndjson\r\nTransfer-Encoding: chunked\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();

        let bytes = body.as_bytes();
        let piece = bytes.len().div_ceil(pieces.max(1));
        for part in bytes.chunks(piece.max(1)) {
            sock.write_all(format!("{:x}\r\n", part.len()).as_bytes())
                .await
                .unwrap();
            sock.write_all(part).await.unwrap();
            sock.write_all(b"\r\n").await.unwrap();
            // Force the chunks onto separate reads on the client side.
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        sock.write_all(b"0\r\n\r\n").await.unwrap();
        sock.flush().await.unwrap();
    });

    format!("http://{addr}")
}

const THREE_TOKENS: &str = concat!(
    r#"{"model":"m","response":"Hello","done":false}"#,
    "\n",
    r#"{"model":"m","response":" there","done":false}"#,
    "\n",
    r#"{"model":"m","response":".","done":false}"#,
    "\n",
    r#"{"model":"m","response":"","done":true,"done_reason":"stop","prompt_eval_count":28,"eval_count":3}"#,
    "\n",
);

#[tokio::test]
async fn streams_tokens_and_reports_counts() {
    let host = mock_ollama(THREE_TOKENS, 3, "200 OK").await;
    let backend = OllamaBackend::new(host, "m");

    let mut seen = Vec::new();
    let mut sink = |t: &str| {
        seen.push(t.to_string());
        true
    };

    let result = backend.infer("hi", 16, &mut sink).await.unwrap();

    assert_eq!(result.text, "Hello there.");
    assert_eq!(seen, vec!["Hello", " there", "."]);
    // Counts come from the final message, not from counting chunks.
    assert_eq!(result.tokens_in, 28);
    assert_eq!(result.tokens_out, 3);
}

#[tokio::test]
async fn a_json_line_split_across_chunk_boundaries_is_reassembled() {
    // 40 pieces over ~250 bytes guarantees lines are torn apart. A client that
    // parses each chunk as a line fails here and only here.
    let host = mock_ollama(THREE_TOKENS, 40, "200 OK").await;
    let backend = OllamaBackend::new(host, "m");

    let mut sink = |_: &str| true;
    let result = backend.infer("hi", 16, &mut sink).await.unwrap();

    assert_eq!(result.text, "Hello there.");
    assert_eq!(result.tokens_out, 3);
}

#[tokio::test]
async fn several_json_lines_in_one_chunk_are_all_handled() {
    // The opposite boundary case: everything arrives at once.
    let host = mock_ollama(THREE_TOKENS, 1, "200 OK").await;
    let backend = OllamaBackend::new(host, "m");

    let mut seen = Vec::new();
    let mut sink = |t: &str| {
        seen.push(t.to_string());
        true
    };

    let result = backend.infer("hi", 16, &mut sink).await.unwrap();
    assert_eq!(seen.len(), 3, "all three tokens must surface");
    assert_eq!(result.text, "Hello there.");
}

#[tokio::test]
async fn a_stop_verdict_ends_generation_early() {
    let host = mock_ollama(THREE_TOKENS, 4, "200 OK").await;
    let backend = OllamaBackend::new(host, "m");

    let mut seen = Vec::new();
    let mut sink = |t: &str| {
        seen.push(t.to_string());
        // Stop after the first token.
        false
    };

    let result = backend.infer("hi", 16, &mut sink).await.unwrap();

    assert_eq!(seen, vec!["Hello"], "must not keep consuming after Stop");
    assert_eq!(result.text, "Hello");
    // The final message was never reached, so the count is what was observed
    // rather than what Ollama would have reported.
    assert_eq!(result.tokens_out, 1);
}

#[tokio::test]
async fn an_http_error_is_reported_with_its_body() {
    let host = mock_ollama(r#"{"error":"model not found"}"#, 1, "404 Not Found").await;
    let backend = OllamaBackend::new(host, "nope");

    let mut sink = |_: &str| true;
    let err = backend.infer("hi", 16, &mut sink).await.unwrap_err();
    let msg = err.to_string();

    assert!(msg.contains("404"), "status must be reported: {msg}");
    assert!(
        msg.contains("model not found"),
        "body must be included: {msg}"
    );
}

#[tokio::test]
async fn an_error_inside_the_stream_is_surfaced() {
    // Ollama can answer 200 and then report a problem in the body.
    let body = concat!(r#"{"error":"something went wrong"}"#, "\n");
    let host = mock_ollama(body, 1, "200 OK").await;
    let backend = OllamaBackend::new(host, "m");

    let mut sink = |_: &str| true;
    let err = backend.infer("hi", 16, &mut sink).await.unwrap_err();
    assert!(err.to_string().contains("something went wrong"), "{err}");
}

#[tokio::test]
async fn an_unreachable_host_explains_itself() {
    // Port 1 is reserved and nothing listens there.
    let backend = OllamaBackend::new("http://127.0.0.1:1", "m");

    let mut sink = |_: &str| true;
    let err = backend.infer("hi", 16, &mut sink).await.unwrap_err();
    let msg = err.to_string();

    assert!(
        msg.contains("Is it running?"),
        "a connection failure should say what is probably wrong: {msg}"
    );
    assert!(
        msg.contains("OLLAMA_HOST"),
        "and how to point elsewhere: {msg}"
    );
}

#[tokio::test]
async fn a_trailing_slash_on_the_host_does_not_produce_a_double_slash() {
    // `OLLAMA_HOST=http://localhost:11434/` is an easy thing to set, and
    // `//api/generate` is rejected.
    let host = mock_ollama(THREE_TOKENS, 2, "200 OK").await;
    let backend = OllamaBackend::new(format!("{host}/"), "m");

    let mut sink = |_: &str| true;
    assert!(backend.infer("hi", 16, &mut sink).await.is_ok());
}

#[tokio::test]
async fn model_name_is_reported_for_usage_accounting() {
    let backend = OllamaBackend::new("http://localhost:11434", "llama3.2:1b");
    assert_eq!(backend.model_name(), "llama3.2:1b");
}

/// Talks to a real Ollama. Run with `-- --ignored`.
///
/// Asserts only the shape of the response, never its wording: a language model
/// is not a deterministic function, and a test that expects particular words
/// fails for reasons that have nothing to do with this code.
#[tokio::test]
#[ignore = "requires a running Ollama with llama3.2:1b"]
async fn talks_to_a_real_ollama() {
    let backend = OllamaBackend::new(OllamaBackend::host_from_env(), "llama3.2:1b");

    let mut seen = Vec::new();
    let mut sink = |t: &str| {
        seen.push(t.to_string());
        true
    };

    let result = backend
        .infer("Reply with exactly the word: ok", 16, &mut sink)
        .await
        .expect("is ollama running with llama3.2:1b pulled?");

    assert!(!result.text.is_empty(), "the model produced no text");
    assert!(
        !seen.is_empty(),
        "tokens must be streamed, not just returned"
    );
    assert_eq!(
        seen.concat(),
        result.text,
        "streamed tokens must reconstruct the returned text exactly"
    );
    assert!(result.tokens_in > 0, "prompt tokens must be counted");
    assert!(result.tokens_out > 0, "generated tokens must be counted");
}

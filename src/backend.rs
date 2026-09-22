//! Protocol adapter for `openai-compatible`, `chat` operation only.
//!
//! Architecture decision: the core embeds knowledge
//! of the `OpenAI` **protocol** (request body shape, response extraction
//! path), not business semantics. `chat` is the only operation supported in
//! phase 1; the other `OpenAI` operations (`embeddings`,
//! `audio_transcriptions`, ...) will extend this adapter without touching
//! the domain model (`config.rs`).

use std::time::Duration;

use serde_json::Value;

/// Maximum delay granted to a `chat` request before failure, when the
/// backend declares no `[timeouts]` override. 120s covers a full
/// `max_tokens` generation on a slow accelerator (observed: ~80s for 1024
/// tokens on an NPU) with headroom; a backend needing more overrides it via
/// `[timeouts].request_secs`.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// The timeout to grant a `chat` request against `backend`: its
/// `[timeouts].request_secs` override if declared, [`REQUEST_TIMEOUT`]
/// otherwise.
fn effective_timeout(backend: &crate::config::Backend) -> Duration {
    backend
        .timeouts
        .as_ref()
        .map_or(REQUEST_TIMEOUT, |timeouts| {
            Duration::from_secs(timeouts.request_secs)
        })
}

/// Maximum number of characters of the response body included in an error
/// message, to stay diagnosable without flooding stderr.
const ERROR_BODY_TRUNCATE_AT: usize = 500;

/// Executes the `chat` operation of the given `model` against `backend`, with `prompt`.
///
/// `base_url` is passed in rather than read from `backend`: a
/// `port = "auto"` backend's own `base_url` still carries
/// `{{ backend.port }}` after loading, and only `builtin::resolve_base_url`
/// can complete it. Taking it as a parameter makes it impossible to reach
/// the network with an unresolved one by accident.
pub fn chat(
    backend: &crate::config::Backend,
    model: &crate::config::Model,
    base_url: &str,
    prompt: &str,
    logger: crate::log::Logger,
) -> crate::Result<String> {
    let operation = backend.operations.get(&model.operation).ok_or_else(|| {
        crate::Error::Config(format!(
            "backend \"{}\" does not expose operation \"{}\" (available operations: {})",
            backend.id,
            model.operation,
            crate::error::format_available(backend.operations.keys())
        ))
    })?;

    let url = join_url(base_url, &operation.path);
    let body = build_chat_request(&model.model, prompt, &model.generation);

    let timeout = effective_timeout(backend);

    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        // A non-2xx status must stay readable: we want its body in the
        // error message, not an opaque ureq error.
        .http_status_as_error(false)
        .build()
        .new_agent();

    logger.info(&format!(
        "POST {url} (model \"{}\", timeout {} s)",
        model.model,
        timeout.as_secs()
    ));

    let started = std::time::Instant::now();
    let mut response = agent.post(&url).send_json(&body).map_err(|err| {
        crate::Error::Backend(format!(
            "request to backend \"{}\" ({url}) failed: {err}",
            backend.id
        ))
    })?;

    let status = response.status();
    let response_text = response.body_mut().read_to_string().map_err(|err| {
        crate::Error::Backend(format!(
            "reading the response from backend \"{}\" ({url}) failed: {err}",
            backend.id
        ))
    })?;

    logger.info(&format!(
        "backend \"{}\" answered {status} in {} ms, {} characters",
        backend.id,
        started.elapsed().as_millis(),
        response_text.chars().count()
    ));

    if !status.is_success() {
        return Err(crate::Error::Backend(format!(
            "backend \"{}\" ({url}) responded with status {status}: {}",
            backend.id,
            truncate(&response_text, ERROR_BODY_TRUNCATE_AT)
        )));
    }

    let response_json: Value = serde_json::from_str(&response_text).map_err(|err| {
        crate::Error::Backend(format!(
            "response from backend \"{}\" ({url}) unreadable as JSON: {err}; body received: {}",
            backend.id,
            truncate(&response_text, ERROR_BODY_TRUNCATE_AT)
        ))
    })?;

    extract_chat_content(&response_json).ok_or_else(|| {
        crate::Error::Backend(format!(
            "response from backend \"{}\" ({url}) has no usable content (expected \
             choices[0].message.content); body received: {}",
            backend.id,
            truncate(&response_text, ERROR_BODY_TRUNCATE_AT)
        ))
    })
}

/// Joins a base URL and an operation path without doubling or losing the `/`.
fn join_url(base_url: &str, path: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if path.starts_with('/') {
        format!("{base}{path}")
    } else {
        format!("{base}/{path}")
    }
}

/// Builds the `chat/completions` request body in `OpenAI` format.
///
/// `temperature` and `max_tokens` are only inserted if they are `Some`: no
/// `null` value is serialized for an absent field.
fn build_chat_request(model: &str, prompt: &str, generation: &crate::config::Generation) -> Value {
    let mut body = serde_json::json!({
        "model": model,
        "messages": [
            { "role": "user", "content": prompt }
        ]
    });

    if let Value::Object(map) = &mut body {
        if let Some(temperature) = generation.temperature {
            // Going through `f64` directly (`Value::from(f32)`) reintroduces
            // f32 binary noise (e.g. 0.7 -> 0.699999988079071). Going
            // through the shortest textual representation of the f32 (the
            // one `Display`/`ToString` produce) gives an f64 that displays
            // the same value as the one written in config.
            if let Some(number) = serde_json::Number::from_f64(f64_from_f32_text(temperature)) {
                map.insert("temperature".to_string(), Value::Number(number));
            }
        }
        if let Some(max_tokens) = generation.max_tokens {
            map.insert("max_tokens".to_string(), Value::from(max_tokens));
        }
    }

    body
}

/// Converts an `f32` to `f64` via its shortest textual representation, to
/// avoid exposing the binary noise introduced by a direct `f32 -> f64`
/// widening (`0.7_f32 as f64` != `0.7_f64`).
fn f64_from_f32_text(value: f32) -> f64 {
    value
        .to_string()
        .parse()
        .unwrap_or_else(|_| f64::from(value))
}

/// Extracts `choices[0].message.content` from a `chat/completions` response.
fn extract_chat_content(response: &Value) -> Option<String> {
    response
        .get("choices")?
        .as_array()?
        .first()?
        .get("message")?
        .get("content")?
        .as_str()
        .map(str::to_string)
}

/// Truncates `s` to `max_chars` characters, respecting UTF-8 boundaries.
fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut truncated: String = s.chars().take(max_chars).collect();
    truncated.push('…');
    truncated
}

#[cfg(test)]
#[allow(clippy::expect_used)] // tolerated in tests (cf. Cargo.toml [lints.clippy]); same
// convention as in lib.rs/command.rs/config.rs/input.rs.
mod tests {
    use super::*;
    use crate::config::{Generation, Operation};
    use std::collections::HashMap;

    /// Minimal backend fixture for [`effective_timeout`] tests: only
    /// `timeouts` varies between cases.
    fn backend_with_timeouts(timeouts: Option<crate::config::Timeouts>) -> crate::config::Backend {
        crate::config::Backend {
            id: "stub".to_string(),
            base_url: "http://127.0.0.1:0".to_string(),
            kind: "openai-compatible".to_string(),
            operations: HashMap::new(),
            port: None,
            runtime: None,
            docker: None,
            timeouts,
            source: std::path::PathBuf::new(),
        }
    }

    #[test]
    fn effective_timeout_falls_back_to_default_when_unset() {
        let backend = backend_with_timeouts(None);
        assert_eq!(effective_timeout(&backend), REQUEST_TIMEOUT);
    }

    #[test]
    fn effective_timeout_uses_backend_override_when_set() {
        let backend = backend_with_timeouts(Some(crate::config::Timeouts { request_secs: 5 }));
        assert_eq!(effective_timeout(&backend), Duration::from_secs(5));
    }

    #[test]
    fn join_url_without_trailing_slash_on_base() {
        assert_eq!(
            join_url("http://127.0.0.1:8000", "/v3/chat/completions"),
            "http://127.0.0.1:8000/v3/chat/completions"
        );
    }

    #[test]
    fn join_url_with_trailing_slash_on_base() {
        assert_eq!(
            join_url("http://127.0.0.1:8000/", "/v3/chat/completions"),
            "http://127.0.0.1:8000/v3/chat/completions"
        );
    }

    #[test]
    fn join_url_with_path_missing_leading_slash() {
        assert_eq!(
            join_url("http://127.0.0.1:8000", "v3/chat/completions"),
            "http://127.0.0.1:8000/v3/chat/completions"
        );
    }

    #[test]
    fn build_chat_request_without_generation_options() {
        let generation = Generation {
            temperature: None,
            max_tokens: None,
        };
        let body = build_chat_request("qwen-2.5-1.5b", "hello", &generation);

        assert_eq!(body["model"], "qwen-2.5-1.5b");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "hello");
        assert!(body.get("temperature").is_none());
        assert!(body.get("max_tokens").is_none());
    }

    #[test]
    fn build_chat_request_with_generation_options() {
        let generation = Generation {
            temperature: Some(0.0),
            max_tokens: Some(512),
        };
        let body = build_chat_request("qwen-2.5-1.5b", "hello", &generation);

        assert_eq!(body["temperature"], 0.0);
        assert_eq!(body["max_tokens"], 512);
    }

    #[test]
    fn build_chat_request_temperature_has_no_f32_widening_noise() {
        // `0.7_f32 as f64` != `0.7_f64` (binary noise): the emitted body
        // must display the same value as the one written in config, not
        // its widened noisy version (e.g. 0.699999988079071).
        let generation = Generation {
            temperature: Some(0.7),
            max_tokens: None,
        };
        let body = build_chat_request("qwen-2.5-1.5b", "hello", &generation);

        assert_eq!(body["temperature"].to_string(), "0.7");
    }

    #[test]
    fn extract_chat_content_nominal() {
        let response = serde_json::json!({
            "choices": [
                { "message": { "role": "assistant", "content": "response" } }
            ]
        });
        assert_eq!(
            extract_chat_content(&response),
            Some("response".to_string())
        );
    }

    #[test]
    fn extract_chat_content_missing_choices() {
        let response = serde_json::json!({ "error": "boom" });
        assert_eq!(extract_chat_content(&response), None);
    }

    #[test]
    fn extract_chat_content_empty_choices() {
        let response = serde_json::json!({ "choices": [] });
        assert_eq!(extract_chat_content(&response), None);
    }

    #[test]
    fn truncate_keeps_short_string_unchanged() {
        assert_eq!(truncate("short", 500), "short");
    }

    #[test]
    fn truncate_cuts_long_string() {
        let long = "a".repeat(600);
        let truncated = truncate(&long, 500);
        assert_eq!(truncated.chars().count(), 501); // 500 + '…'
        assert!(truncated.ends_with('…'));
    }

    /// Integration test against a stubbed HTTP listener: covers `chat()`
    /// end to end, with no extra
    /// dependency (just `std::net`/`std::thread`). None of this module's
    /// other tests exercise `chat()` itself, only its private functions:
    /// this was the module's most notable coverage gap.
    #[test]
    fn chat_end_to_end_against_stubbed_http_server() {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind of the stubbed listener");
        let addr = listener
            .local_addr()
            .expect("local address of the listener");

        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accepting the connection");
            let mut reader = BufReader::new(stream.try_clone().expect("cloning the TCP stream"));

            // Drains the request headers up to the empty line, then the
            // body announced by Content-Length; its exact content does not
            // need to be checked for this end-to-end test.
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("reading a header line");
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = value.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; content_length];
            reader
                .read_exact(&mut body)
                .expect("reading the request body");

            let response_body =
                r#"{"choices":[{"message":{"role":"assistant","content":"stubbed reply"}}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            let mut stream = stream;
            stream
                .write_all(response.as_bytes())
                .expect("writing the stubbed response");
        });

        let backend = crate::config::Backend {
            id: "stub".to_string(),
            base_url: format!("http://{addr}"),
            kind: "openai-compatible".to_string(),
            operations: [(
                "chat".to_string(),
                Operation {
                    method: "POST".to_string(),
                    path: "/v1/chat/completions".to_string(),
                },
            )]
            .into_iter()
            .collect(),
            port: None,
            runtime: None,
            docker: None,
            timeouts: None,
            source: std::path::PathBuf::new(),
        };
        let model = crate::config::Model {
            id: "test-model".to_string(),
            backend: "stub".to_string(),
            operation: "chat".to_string(),
            model: "test-model".to_string(),
            fallback: None,
            generation: Generation::default(),
        };

        let result = chat(
            &backend,
            &model,
            &backend.base_url.clone(),
            "hello",
            crate::log::Logger::new(crate::log::Level::Error),
        )
        .expect("chat() must succeed against the stubbed listener");
        assert_eq!(result, "stubbed reply");

        server.join().expect("the server thread must not panic");
    }
}

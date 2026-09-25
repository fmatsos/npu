//! Protocol adapter for `openai-compatible`, `chat` operation only.
//!
//! Architecture decision: the core embeds knowledge
//! of the `OpenAI` **protocol** (request body shape, response extraction
//! path), not business semantics. `chat` is the only operation currently
//! supported; the other `OpenAI` operations (`embeddings`,
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

/// The request builder for `operation`. Type and method were validated at
/// load time; these `match`es are what make a new type or method a compile
/// error here rather than a request sent the wrong way.
fn request_builder(
    agent: &ureq::Agent,
    url: &str,
    backend: &crate::config::Backend,
    operation: &crate::config::Operation,
) -> crate::Result<ureq::RequestBuilder<ureq::typestate::WithBody>> {
    let unvalidated = || {
        crate::Error::Config(crate::error::ConfigError::bare(
            Some(&backend.id),
            format!(
                "backend \"{}\": type \"{}\" or method \"{}\" was not validated",
                backend.id, backend.kind, operation.method
            ),
        ))
    };
    match crate::config::BackendKind::parse(&backend.kind) {
        Some(crate::config::BackendKind::OpenAiCompatible) => {}
        None => return Err(unvalidated()),
    }
    match crate::config::Method::parse(&operation.method) {
        Some(crate::config::Method::Post) => Ok(agent.post(url)),
        None => Err(unvalidated()),
    }
}

/// A fully built `chat` request, before it is sent: the resolved URL, the
/// resolved headers (name -> value, `{{ env.NAME }}` already substituted)
/// and the JSON body. Built by [`build_request`], the single constructor
/// [`chat`] and `--dry-run` (`exec::execute_business_command`) both go
/// through, so the body a dry run shows is byte-identical to the one an
/// actual call would send.
#[derive(Debug)]
pub struct PreparedRequest {
    pub url: String,
    pub headers: std::collections::BTreeMap<String, String>,
    pub body: Value,
}

/// Builds the request `chat` would send to `model` on `backend`, without
/// sending it: the URL (joined from `base_url` and the operation's path,
/// UNRESOLVED — a `port = "auto"` backend's `{{ backend.port }}` placeholder
/// is left as `base_url` carries it, since resolving it means reaching
/// Docker, which `--dry-run` must never do), the resolved headers, and the
/// JSON body (`response_format` sent only when `backend.structured_output`
/// is declared).
///
/// # Errors
/// `Error::Config` when `backend` does not expose `model.operation`.
pub fn build_request(
    backend: &crate::config::Backend,
    model: &crate::config::Model,
    base_url: &str,
    request: &Request<'_>,
) -> crate::Result<PreparedRequest> {
    let Request {
        messages,
        schema,
        on_token: _,
        headers,
        generation,
    } = *request;
    let operation = backend.operations.get(&model.operation).ok_or_else(|| {
        crate::Error::Config(crate::error::ConfigError::bare(
            Some(&backend.id),
            format!(
                "backend \"{}\" does not expose operation \"{}\" (available operations: {})",
                backend.id,
                model.operation,
                crate::error::format_available(backend.operations.keys())
            ),
        ))
    })?;

    let url = join_url(base_url, &operation.path);
    let schema = schema.filter(|_| backend.structured_output);
    let body = build_chat_request(&model.model, messages, generation, schema);

    Ok(PreparedRequest {
        url,
        headers: headers.clone(),
        body,
    })
}

/// Executes the `chat` operation of the given `model` against `backend`, with `prompt`.
///
/// `schema` is the command's output schema, if any: sent as
/// `response_format` only when `backend.structured_output` is declared,
/// ignored otherwise (the caller validates the answer either way).
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
    request: &Request<'_>,
    logger: crate::log::Logger,
) -> crate::Result<ChatAnswer> {
    let Request {
        messages: _,
        schema,
        on_token,
        headers,
        generation,
    } = *request;
    let operation = backend.operations.get(&model.operation).ok_or_else(|| {
        crate::Error::Config(crate::error::ConfigError::bare(
            Some(&backend.id),
            format!(
                "backend \"{}\" does not expose operation \"{}\" (available operations: {})",
                backend.id,
                model.operation,
                crate::error::format_available(backend.operations.keys())
            ),
        ))
    })?;

    let PreparedRequest { url, body, .. } = build_request(backend, model, base_url, request)?;
    let mut body = body;
    let backend_err = |message: String| {
        crate::Error::Backend(crate::error::BackendError::at_url(
            &backend.id,
            &url,
            message,
        ))
    };
    let schema = schema.filter(|_| backend.structured_output);
    if on_token.is_some()
        && let Value::Object(map) = &mut body
    {
        map.insert("stream".to_string(), Value::Bool(true));
    }

    let timeout = effective_timeout(backend);

    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        // A non-2xx status must stay readable: we want its body in the
        // error message, not an opaque ureq error.
        .http_status_as_error(false)
        .build()
        .new_agent();

    logger.info(&format!(
        "POST {url} (model \"{}\", timeout {} s{}{}{})",
        model.model,
        timeout.as_secs(),
        if schema.is_some() {
            ", output schema sent as response_format"
        } else {
            ""
        },
        if headers.is_empty() {
            String::new()
        } else {
            format!(
                ", with headers: {}",
                headers.keys().cloned().collect::<Vec<_>>().join(", ")
            )
        },
        generation_keys_suffix(generation)
    ));

    let started = std::time::Instant::now();
    let mut post = request_builder(&agent, &url, backend, operation)?;
    for (name, value) in headers {
        post = post.header(name.as_str(), value.as_str());
    }
    let mut response = post.send_json(&body).map_err(|err| {
        backend_err(format!(
            "request to backend \"{}\" ({url}) failed: {err}",
            backend.id
        ))
    })?;

    let status = response.status();
    if let (Some(on_token), true) = (on_token, status.is_success()) {
        let stream_result =
            read_stream(response.body_mut().as_reader(), on_token).map_err(|err| {
                backend_err(format!(
                    "reading the streamed response from backend \"{}\" ({url}) failed: {err}",
                    backend.id
                ))
            })?;
        logger.info(&format!(
            "backend \"{}\" streamed {status} in {} ms, {} characters{}",
            backend.id,
            started.elapsed().as_millis(),
            stream_result.content.chars().count(),
            usage_suffix(stream_result.usage.as_ref())
        ));
        return Ok(ChatAnswer {
            content: stream_result.content,
            finish_reason: stream_result.finish_reason,
            usage: stream_result.usage,
        });
    }
    let response_text = response.body_mut().read_to_string().map_err(|err| {
        backend_err(format!(
            "reading the response from backend \"{}\" ({url}) failed: {err}",
            backend.id
        ))
    })?;

    handle_non_streamed_response(
        &backend.id,
        &url,
        status,
        &response_text,
        started.elapsed(),
        logger,
    )
}

/// Handles the non-streamed tail of [`chat`]: status check, JSON parsing,
/// content/`finish_reason`/`usage` extraction and the "answered" info log.
/// Split out of `chat` only to keep it under the crate's line-count lint —
/// no behavior is different from what used to be inlined there.
fn handle_non_streamed_response(
    backend_id: &str,
    url: &str,
    status: ureq::http::StatusCode,
    response_text: &str,
    elapsed: std::time::Duration,
    logger: crate::log::Logger,
) -> crate::Result<ChatAnswer> {
    let backend_err = |message: String| {
        crate::Error::Backend(crate::error::BackendError::at_url(backend_id, url, message))
    };

    if !status.is_success() {
        logger.info(&format!(
            "backend \"{backend_id}\" answered {status} in {} ms, {} characters",
            elapsed.as_millis(),
            response_text.chars().count()
        ));
        return Err(crate::Error::Backend(
            crate::error::BackendError::at_status(
                backend_id,
                url,
                status.as_u16(),
                format!(
                    "backend \"{backend_id}\" ({url}) responded with status {status}: {}",
                    truncate(response_text, ERROR_BODY_TRUNCATE_AT)
                ),
            ),
        ));
    }

    let response_json: Value = serde_json::from_str(response_text).map_err(|err| {
        backend_err(format!(
            "response from backend \"{backend_id}\" ({url}) unreadable as JSON: {err}; body \
             received: {}",
            truncate(response_text, ERROR_BODY_TRUNCATE_AT)
        ))
    })?;

    let content = extract_chat_content(&response_json).ok_or_else(|| {
        backend_err(format!(
            "response from backend \"{backend_id}\" ({url}) has no usable content (expected \
             choices[0].message.content); body received: {}",
            truncate(response_text, ERROR_BODY_TRUNCATE_AT)
        ))
    })?;
    let finish_reason = extract_finish_reason(&response_json);
    let usage = response_json.get("usage").and_then(parse_usage);

    logger.info(&format!(
        "backend \"{backend_id}\" answered {status} in {} ms, {} characters{}",
        elapsed.as_millis(),
        response_text.chars().count(),
        usage_suffix(usage.as_ref())
    ));

    Ok(ChatAnswer {
        content,
        finish_reason,
        usage,
    })
}

/// Token usage reported by the backend for one `chat` call, when present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

/// The answer to a `chat` call: its content, the reason generation stopped
/// (when the backend reports one — `"stop"`, `"length"`, ...), and token
/// usage (when the backend reports it).
#[derive(Debug, Clone)]
pub struct ChatAnswer {
    pub content: String,
    pub finish_reason: Option<String>,
    pub usage: Option<Usage>,
}

/// Parses a `usage` object (`{"prompt_tokens": N, "completion_tokens": M,
/// ...}`) into a [`Usage`]. `None` if either field is missing or not an
/// integer — a partial/malformed `usage` object is treated as absent rather
/// than guessed at.
fn parse_usage(usage: &Value) -> Option<Usage> {
    Some(Usage {
        prompt_tokens: usage.get("prompt_tokens")?.as_u64()?,
        completion_tokens: usage.get("completion_tokens")?.as_u64()?,
    })
}

/// The ", N prompt + M completion tokens" suffix appended to the "answered"/
/// "streamed" info log line when usage is known; empty otherwise.
fn usage_suffix(usage: Option<&Usage>) -> String {
    usage.map_or_else(String::new, |usage| {
        format!(
            ", {} prompt + {} completion tokens",
            usage.prompt_tokens, usage.completion_tokens
        )
    })
}

/// Extracts `choices[0].finish_reason` from a `chat/completions` response,
/// when present and a string.
fn extract_finish_reason(response: &Value) -> Option<String> {
    response
        .pointer("/choices/0/finish_reason")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// The result of reading a full event stream: the concatenated answer, the
/// `finish_reason` of its last event that carried one, and `usage` when a
/// (possibly final, otherwise-empty) chunk carried it.
#[derive(Debug)]
struct StreamResult {
    content: String,
    finish_reason: Option<String>,
    usage: Option<Usage>,
}

/// Reads an `OpenAI` `chat/completions` event stream (`data: {...}` lines,
/// ended by `data: [DONE]` or the end of the body), hands each
/// `choices[0].delta.content` to `on_token` as it arrives, and returns the
/// whole answer plus `finish_reason`/`usage` when the stream carries them.
/// Any other line — a blank separator, a comment, an event without content —
/// is skipped.
///
/// Two cases end the stream with an `Err` (mapped to `Error::Backend` by the
/// caller), rather than a partial or empty success:
/// - an event carrying a top-level `error` object (the server reporting a
///   failure mid-stream): tokens already handed to `on_token` before this
///   point stay delivered, but the call as a whole fails, so no fallback is
///   attempted once anything has been emitted;
/// - a stream that ends with no content at all AND no `finish_reason`: a
///   `chat/completions` stream reporting nothing usable, indistinguishable
///   from a broken connection, must not be reported as a successful empty
///   answer.
fn read_stream(
    reader: impl std::io::Read,
    on_token: &dyn Fn(&str),
) -> std::io::Result<StreamResult> {
    use std::io::BufRead;
    let mut answer = String::new();
    let mut finish_reason = None;
    let mut usage = None;
    for line in std::io::BufReader::new(reader).lines() {
        let line = line?;
        let Some(data) = line.strip_prefix("data:").map(str::trim) else {
            continue;
        };
        if data == "[DONE]" {
            break;
        }
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        if let Some(error) = event.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            return Err(std::io::Error::other(format!(
                "server reported an error mid-stream: {message}"
            )));
        }
        let content = event
            .pointer("/choices/0/delta/content")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !content.is_empty() {
            on_token(content);
            answer.push_str(content);
        }
        if let Some(reason) = event
            .pointer("/choices/0/finish_reason")
            .and_then(Value::as_str)
        {
            finish_reason = Some(reason.to_string());
        }
        if let Some(reported) = event.get("usage").and_then(parse_usage) {
            usage = Some(reported);
        }
    }
    if answer.is_empty() && finish_reason.is_none() {
        return Err(std::io::Error::other(
            "stream ended with no content and no finish_reason",
        ));
    }
    Ok(StreamResult {
        content: answer,
        finish_reason,
        usage,
    })
}

/// One chat message: `role` is `"system"`, `"user"` or `"assistant"`.
#[derive(Debug, Clone)]
pub struct Message {
    pub role: String,
    pub content: String,
}

impl Message {
    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
        }
    }
}

/// What one chat call asks: the messages (system?, examples×[user,
/// assistant], the rendered body as the final user message — see
/// `exec::execute_business_command`), the schema constraining the answer
/// (sent only to a backend declaring `structured_output`), and where the
/// answer's tokens go as they arrive — `None` waits for the whole answer.
#[derive(Clone, Copy)]
pub struct Request<'a> {
    pub messages: &'a [Message],
    pub schema: Option<&'a Value>,
    pub on_token: Option<&'a dyn Fn(&str)>,
    /// Resolved `[headers]` of the backend being called (name -> resolved
    /// value, `{{ env.NAME }}` already substituted): see
    /// `config::resolve_headers`. Never logged.
    pub headers: &'a std::collections::BTreeMap<String, String>,
    /// The generation parameters for THIS call: the model's own
    /// `[generation]` already merged with the command's override, if any
    /// (`config::Generation::merged`) — never `model.generation` read
    /// directly, so a command override reaches the request whichever
    /// model (primary or fallback) actually answers.
    pub generation: &'a crate::config::Generation,
}

// `&dyn Fn` cannot derive `Debug`: the closure has nothing to print. Header
// VALUES are deliberately excluded, even in Debug output: only their names.
impl std::fmt::Debug for Request<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request")
            .field("message_count", &self.messages.len())
            .field("schema", &self.schema)
            .field("streamed", &self.on_token.is_some())
            .field("header_names", &self.headers.keys().collect::<Vec<_>>())
            .finish()
    }
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
/// `null` value is serialized for an absent field. Likewise
/// `response_format`, only for `Some(schema)`.
fn build_chat_request(
    model: &str,
    messages: &[Message],
    generation: &crate::config::Generation,
    schema: Option<&Value>,
) -> Value {
    let messages: Vec<Value> = messages
        .iter()
        .map(|message| serde_json::json!({ "role": message.role, "content": message.content }))
        .collect();
    let mut body = serde_json::json!({
        "model": model,
        "messages": messages
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
        if let Some(seed) = generation.seed {
            map.insert("seed".to_string(), Value::from(seed));
        }
        if let Some(top_p) = generation.top_p {
            // Same f32-via-text treatment as `temperature`, see above.
            if let Some(number) = serde_json::Number::from_f64(f64_from_f32_text(top_p)) {
                map.insert("top_p".to_string(), Value::Number(number));
            }
        }
        if let Some(stop) = &generation.stop {
            map.insert("stop".to_string(), Value::from(stop.clone()));
        }
        if let Some(schema) = schema {
            map.insert("response_format".to_string(), response_format(schema));
        }
        // `extra` last: every key of it is forwarded verbatim, at the top
        // level, after the typed keys above — `generation_errors` already
        // rejected any key colliding with one of them, at load time.
        if let Some(extra) = &generation.extra {
            for (key, value) in crate::config::extra_to_json(extra) {
                map.insert(key, value);
            }
        }
    }

    body
}

/// The `", generation: temperature, seed, extra.chat_template_kwargs"` suffix
/// appended to the `POST` trace line: the NAMES of the generation keys
/// actually sent (typed keys present, plus each top-level `extra` key
/// prefixed `extra.`), never their values — an `extra` value may be a
/// secret someday, same discipline as `[headers]`. Empty when nothing in
/// `generation` is set.
fn generation_keys_suffix(generation: &crate::config::Generation) -> String {
    let mut keys = Vec::new();
    if generation.temperature.is_some() {
        keys.push("temperature".to_string());
    }
    if generation.max_tokens.is_some() {
        keys.push("max_tokens".to_string());
    }
    if generation.seed.is_some() {
        keys.push("seed".to_string());
    }
    if generation.top_p.is_some() {
        keys.push("top_p".to_string());
    }
    if generation.stop.is_some() {
        keys.push("stop".to_string());
    }
    if let Some(extra) = &generation.extra {
        keys.extend(extra.keys().map(|key| format!("extra.{key}")));
    }
    if keys.is_empty() {
        String::new()
    } else {
        format!(", generation: {}", keys.join(", "))
    }
}

/// The `OpenAI` `response_format` constraining the answer to `schema`.
fn response_format(schema: &Value) -> Value {
    serde_json::json!({
        "type": "json_schema",
        "json_schema": { "name": "output", "schema": schema }
    })
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
            structured_output: false,
            headers: std::collections::BTreeMap::new(),
            source: std::path::PathBuf::new(),
            max_concurrent: None,
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
            seed: None,
            top_p: None,
            stop: None,
            extra: None,
        };
        let body = build_chat_request(
            "qwen-2.5-1.5b",
            &[Message::user("hello")],
            &generation,
            None,
        );

        assert_eq!(body["model"], "qwen-2.5-1.5b");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "hello");
        assert!(body.get("temperature").is_none());
        assert!(body.get("max_tokens").is_none());
    }

    /// A command declaring neither `system` nor `examples`
    /// must produce a request body BYTE-IDENTICAL to what `npu` sent
    /// before `system` and `examples` existed — a single-element `messages` array holding only
    /// the rendered body as a `user` message.
    #[test]
    fn build_chat_request_without_system_or_examples_is_byte_identical_to_the_plain_body() {
        let body = build_chat_request(
            "qwen-2.5-1.5b",
            &[Message::user("hello")],
            &Generation::default(),
            None,
        );
        assert_eq!(
            serde_json::to_string(&body).expect("body must serialize"),
            r#"{"messages":[{"content":"hello","role":"user"}],"model":"qwen-2.5-1.5b"}"#
        );
    }

    /// `system`, then each example's `[user, assistant]` pair in file
    /// order, then the rendered body as the final `user` message.
    #[test]
    fn build_chat_request_orders_system_then_examples_then_body() {
        let messages = [
            Message {
                role: "system".to_string(),
                content: "be terse".to_string(),
            },
            Message {
                role: "user".to_string(),
                content: "ticket: printer on fire".to_string(),
            },
            Message {
                role: "assistant".to_string(),
                content: r#"{"category":"hardware"}"#.to_string(),
            },
            Message::user("ticket: mouse missing"),
        ];
        let body = build_chat_request("m", &messages, &Generation::default(), None);
        let roles: Vec<&str> = body["messages"]
            .as_array()
            .expect("messages must be an array")
            .iter()
            .map(|m| m["role"].as_str().expect("role must be a string"))
            .collect();
        assert_eq!(roles, ["system", "user", "assistant", "user"]);
        assert_eq!(body["messages"][3]["content"], "ticket: mouse missing");
    }

    #[test]
    fn build_chat_request_with_schema_sends_json_schema_response_format() {
        let schema = serde_json::json!({ "type": "object" });
        let body = build_chat_request(
            "m",
            &[Message::user("hello")],
            &Generation::default(),
            Some(&schema),
        );
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(body["response_format"]["json_schema"]["schema"], schema);
    }

    #[test]
    fn build_chat_request_without_schema_sends_no_response_format() {
        let body = build_chat_request("m", &[Message::user("hello")], &Generation::default(), None);
        assert!(body.get("response_format").is_none());
    }

    #[test]
    fn build_chat_request_with_generation_options() {
        let generation = Generation {
            temperature: Some(0.0),
            max_tokens: Some(512),
            seed: None,
            top_p: None,
            stop: None,
            extra: None,
        };
        let body = build_chat_request(
            "qwen-2.5-1.5b",
            &[Message::user("hello")],
            &generation,
            None,
        );

        assert_eq!(body["temperature"], 0.0);
        assert_eq!(body["max_tokens"], 512);
    }

    /// `seed`, `top_p` and `stop` appear in the body when set, absent
    /// otherwise (same "no key sent when unset" rule as `temperature`).
    #[test]
    fn build_chat_request_sends_seed_top_p_and_stop_when_set() {
        let generation = Generation {
            temperature: None,
            max_tokens: None,
            seed: Some(42),
            top_p: Some(0.9),
            stop: Some(vec!["\n\n".to_string(), "###".to_string()]),
            extra: None,
        };
        let body = build_chat_request("m", &[Message::user("hello")], &generation, None);

        assert_eq!(body["seed"], 42);
        assert_eq!(body["top_p"], 0.9);
        assert_eq!(body["stop"], serde_json::json!(["\n\n", "###"]));
    }

    #[test]
    fn build_chat_request_omits_seed_top_p_and_stop_when_unset() {
        let generation = Generation {
            temperature: None,
            max_tokens: None,
            seed: None,
            top_p: None,
            stop: None,
            extra: None,
        };
        let body = build_chat_request("m", &[Message::user("hello")], &generation, None);

        assert!(body.get("seed").is_none());
        assert!(body.get("top_p").is_none());
        assert!(body.get("stop").is_none());
    }

    /// `top_p` gets the same shortest-text f32->f64 treatment as
    /// `temperature`: no binary widening noise.
    #[test]
    fn build_chat_request_top_p_has_no_f32_widening_noise() {
        let generation = Generation {
            temperature: None,
            max_tokens: None,
            seed: None,
            top_p: Some(0.7),
            stop: None,
            extra: None,
        };
        let body = build_chat_request("m", &[Message::user("hello")], &generation, None);
        assert_eq!(
            serde_json::to_string(&body["top_p"]).expect("must serialize"),
            "0.7"
        );
    }

    /// `[generation.extra]` is forwarded verbatim, at the top level,
    /// alongside the typed keys.
    #[test]
    fn build_chat_request_forwards_extra_verbatim_alongside_typed_keys() {
        let mut kwargs = toml::Table::new();
        kwargs.insert("enable_thinking".to_string(), toml::Value::Boolean(false));
        let mut extra = toml::Table::new();
        extra.insert(
            "chat_template_kwargs".to_string(),
            toml::Value::Table(kwargs),
        );
        let generation = Generation {
            temperature: Some(0.0),
            max_tokens: None,
            seed: None,
            top_p: None,
            stop: None,
            extra: Some(extra),
        };
        let body = build_chat_request("m", &[Message::user("hello")], &generation, None);

        assert_eq!(body["temperature"], 0.0);
        assert_eq!(
            body["chat_template_kwargs"],
            serde_json::json!({ "enable_thinking": false })
        );
    }

    #[test]
    fn build_chat_request_temperature_has_no_f32_widening_noise() {
        // `0.7_f32 as f64` != `0.7_f64` (binary noise): the emitted body
        // must display the same value as the one written in config, not
        // its widened noisy version (e.g. 0.699999988079071).
        let generation = Generation {
            temperature: Some(0.7),
            max_tokens: None,
            seed: None,
            top_p: None,
            stop: None,
            extra: None,
        };
        let body = build_chat_request(
            "qwen-2.5-1.5b",
            &[Message::user("hello")],
            &generation,
            None,
        );

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

        let (header_tx, header_rx) = std::sync::mpsc::channel::<Option<String>>();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accepting the connection");
            let mut reader = BufReader::new(stream.try_clone().expect("cloning the TCP stream"));

            // Drains the request headers up to the empty line, then the
            // body announced by Content-Length; its exact content does not
            // need to be checked for this end-to-end test, except the
            // custom header under test.
            let mut content_length = 0usize;
            let mut authorization = None;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("reading a header line");
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = value.trim().parse().unwrap_or(0);
                }
                if let Some(value) = line
                    .strip_prefix("Authorization:")
                    .or_else(|| line.strip_prefix("authorization:"))
                {
                    authorization = Some(value.trim().to_string());
                }
            }
            header_tx
                .send(authorization)
                .expect("sending the captured header back to the test");
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
            structured_output: false,
            headers: std::collections::BTreeMap::new(),
            source: std::path::PathBuf::new(),
            max_concurrent: None,
        };
        let model = crate::config::Model {
            id: "test-model".to_string(),
            backend: "stub".to_string(),
            operation: "chat".to_string(),
            model: "test-model".to_string(),
            fallback: None,
            generation: Generation::default(),
            source: std::path::PathBuf::new(),
        };

        let sent_headers: std::collections::BTreeMap<String, String> = [(
            "Authorization".to_string(),
            "Bearer secret-token".to_string(),
        )]
        .into_iter()
        .collect();

        let result = chat(
            &backend,
            &model,
            &backend.base_url.clone(),
            &Request {
                messages: &[Message::user("hello")],
                schema: None,
                on_token: None,
                headers: &sent_headers,
                generation: &Generation::default(),
            },
            crate::log::Logger::new(crate::log::Level::Error),
        )
        .expect("chat() must succeed against the stubbed listener");
        assert_eq!(result.content, "stubbed reply");
        assert_eq!(result.finish_reason, None);
        assert!(result.usage.is_none());

        let captured_authorization = header_rx
            .recv()
            .expect("the stub must send back what it captured");
        assert_eq!(
            captured_authorization.as_deref(),
            Some("Bearer secret-token"),
            "the resolved header value must reach the backend"
        );

        server.join().expect("the server thread must not panic");
    }

    #[test]
    fn a_stream_hands_each_delta_over_and_returns_the_whole_answer() {
        let body = "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n\
                    data: {\"choices\":[{\"delta\":{\"content\":\"Bon\"}}]}\n\n\
                    : keep-alive\n\
                    data: {\"choices\":[{\"delta\":{\"content\":\"jour\"}}]}\n\n\
                    data: [DONE]\n\n\
                    data: {\"choices\":[{\"delta\":{\"content\":\"ignored\"}}]}\n";
        let seen = std::cell::RefCell::new(Vec::new());
        let result = read_stream(body.as_bytes(), &|t| seen.borrow_mut().push(t.to_string()))
            .expect("an in-memory stream reads");
        assert_eq!(result.content, "Bonjour");
        assert_eq!(*seen.borrow(), ["Bon", "jour"]);
    }

    /// Spec 1: a top-level `error` event ends the stream with `Err`, and
    /// tokens already handed to `on_token` before it stay delivered — the
    /// caller (`chat_with_fallback`) relies on this to decide the fallback
    /// must NOT be attempted once anything has been emitted.
    #[test]
    fn a_stream_error_event_ends_the_stream_with_err_after_delivering_prior_deltas() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"Bon\"}}]}\n\n\
                    data: {\"choices\":[{\"delta\":{\"content\":\"jour\"}}]}\n\n\
                    data: {\"error\":{\"message\":\"server exploded\"}}\n\n";
        let seen = std::cell::RefCell::new(Vec::new());
        let err = read_stream(body.as_bytes(), &|t| seen.borrow_mut().push(t.to_string()))
            .expect_err("an error event must end the stream with Err");
        assert_eq!(err.kind(), std::io::ErrorKind::Other);
        assert_eq!(*seen.borrow(), ["Bon", "jour"]);
    }

    #[test]
    fn a_stream_finish_reason_length_in_the_last_event_is_captured() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
                    data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n\
                    data: [DONE]\n\n";
        let result = read_stream(body.as_bytes(), &|_| {}).expect("the stream must read");
        assert_eq!(result.content, "hi");
        assert_eq!(result.finish_reason.as_deref(), Some("length"));
    }

    /// Spec 2: a stream delivering no content at all and no `finish_reason`
    /// must be `Err`, never `Ok("")`.
    #[test]
    fn a_stream_with_no_content_and_no_finish_reason_is_err() {
        let body = "data: {\"choices\":[{\"delta\":{}}]}\n\ndata: [DONE]\n\n";
        let err =
            read_stream(body.as_bytes(), &|_| {}).expect_err("an empty stream must be an error");
        assert_eq!(err.kind(), std::io::ErrorKind::Other);
    }

    #[test]
    fn a_stream_usage_from_a_final_chunk_with_empty_choices_is_captured() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\n\
                    data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2}}\n\n\
                    data: [DONE]\n\n";
        let result = read_stream(body.as_bytes(), &|_| {}).expect("the stream must read");
        assert_eq!(result.content, "hi");
        assert_eq!(result.finish_reason.as_deref(), Some("stop"));
        let usage = result.usage.expect("usage must be captured");
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 2);
    }

    #[test]
    fn non_streamed_body_with_finish_reason_and_usage_extracts_both() {
        let response = serde_json::json!({
            "choices": [
                { "message": { "role": "assistant", "content": "hi" }, "finish_reason": "length" }
            ],
            "usage": { "prompt_tokens": 5, "completion_tokens": 7 }
        });
        assert_eq!(extract_chat_content(&response), Some("hi".to_string()));
        assert_eq!(extract_finish_reason(&response), Some("length".to_string()));
        let usage = response
            .get("usage")
            .and_then(parse_usage)
            .expect("usage must parse");
        assert_eq!(usage.prompt_tokens, 5);
        assert_eq!(usage.completion_tokens, 7);
    }
}

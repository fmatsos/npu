//! Business execution: resolving a command's model, rendering its prompt,
//! calling the backend (with the single-hop fallback retry) and writing the
//! answer to stdout under the output contract.

/// Where a streamed answer's tokens go: `(model that answers, token)`.
type TokenSink<'a> = &'a dyn Fn(&str, &str);

/// What [`chat_with_fallback`] asks of the model and, failing it, of its
/// fallback.
#[derive(Clone, Copy)]
pub(crate) struct Ask<'a> {
    pub(crate) messages: &'a [crate::backend::Message],
    pub(crate) schema: Option<&'a serde_json::Value>,
    pub(crate) stream: Option<TokenSink<'a>>,
    /// The command's own `[generation]` override, if declared — merged
    /// (key by key, command wins) onto EACH candidate model's own
    /// `[generation]` inside `chat_with_fallback`'s `call`: the fallback
    /// model uses its own base `[generation]` merged with this SAME
    /// override, never the primary's merged result.
    pub(crate) command_generation: Option<&'a crate::config::Generation>,
}

// `&dyn Fn` cannot derive `Debug`: the closure has nothing to print.
impl std::fmt::Debug for Ask<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ask")
            .field("message_count", &self.messages.len())
            .field("streamed", &self.stream.is_some())
            .finish_non_exhaustive()
    }
}

/// Calls `model`, and on an `Error::Backend` retries ONCE against its
/// declared `fallback`.
///
/// Only `Error::Backend` triggers the retry: an `Error::Config` means the
/// configuration is wrong and retrying elsewhere would hide it, and no other
/// variant can come out of `backend::chat`. The retry is deliberately blind
/// to the REASON of the backend failure — a `400 Input length exceeds the
/// maximum allowed length` from an NPU graph and an unreachable container are
/// indistinguishable in `Error::Backend`, so the primary's failure is logged
/// at `warn` rather than swallowed: a container that has been down all day
/// must not look like a healthy fallback.
///
/// Single hop by construction: the fallback is called through
/// `backend::chat` directly, never through this function, so no chain and no
/// cycle is possible.
pub(crate) fn chat_with_fallback(
    config: &crate::config::Config,
    model: &crate::config::Model,
    backend: &crate::config::Backend,
    ask: &Ask<'_>,
    env: &dyn Fn(&str) -> Option<String>,
    logger: crate::log::Logger,
) -> crate::Result<(crate::backend::ChatAnswer, String)> {
    let Ask {
        messages,
        schema,
        stream,
        command_generation,
    } = *ask;
    // Resolving the URL is part of reaching the backend, not a step before
    // it: a `port = "auto"` backend whose container is down fails here, and
    // that is exactly a case the fallback exists to absorb. Resolving
    // outside this `and_then` would make a stopped NPU container bypass the
    // GPU it was supposed to fall back to.
    // Cleared when this function returns, on every path.
    let spinner =
        crate::progress::Indicator::spinner(&format!("waiting for model \"{}\"", model.id));
    // Whether a streamed token already reached the terminal: from then on
    // the answer is committed to this model, and a failure can no longer
    // be absorbed by the fallback.
    let emitted = std::cell::Cell::new(false);
    let call = |backend: &crate::config::Backend, model: &crate::config::Model| {
        let on_token = |token: &str| {
            if let Some(stream) = stream {
                spinner.clear();
                emitted.set(true);
                stream(&model.id, token);
            }
        };
        let on_token: Option<&dyn Fn(&str)> = stream.map(|_| &on_token as &dyn Fn(&str));
        let headers = crate::config::resolve_headers(backend, env)?;
        // This model's OWN `[generation]` merged with the command's
        // override — recomputed per candidate, never precomputed once for
        // the primary and reused for the fallback (see `Ask`'s doc).
        let generation = crate::config::Generation::merged(&model.generation, command_generation);
        crate::runtime::resolve_base_url(backend, &crate::runtime::docker::runner).and_then(
            |base_url| {
                let request = crate::backend::Request {
                    messages,
                    schema,
                    on_token,
                    headers: &headers,
                    generation: &generation,
                };
                crate::backend::chat(backend, model, &base_url, &request, logger)
            },
        )
    };

    let primary = match call(backend, model) {
        Ok(output) => return Ok((output, model.id.clone())),
        Err(crate::Error::Backend(err)) if !emitted.get() => err.message,
        Err(other) => return Err(other),
    };

    let Some(fallback_id) = model.fallback.as_deref() else {
        return Err(crate::Error::Backend(crate::error::BackendError::at(
            &backend.id,
            primary,
        )));
    };

    // `info`, not `warn`: the fallback is the designed path for a primary
    // that is down or refuses the prompt, and the command still succeeds.
    logger.info(&format!(
        "model \"{}\" failed ({primary}); falling back to model \"{fallback_id}\"",
        model.id
    ));

    // `load_scopes` has already checked that this identifier names a loaded
    // model; `resolve` can still fail on ITS backend, and that is a
    // configuration error which must keep its own exit code rather than be
    // reported as a backend failure.
    let (fallback_model, fallback_backend) = config.resolve(fallback_id)?;
    spinner.set_message(&format!("waiting for fallback model \"{fallback_id}\""));

    call(fallback_backend, fallback_model)
        .map(|output| (output, fallback_id.to_string()))
        .map_err(|err| match err {
            crate::Error::Backend(second) => crate::Error::Backend(crate::error::BackendError::at(
                fallback_id,
                format!(
                    "model \"{}\" failed ({primary}), and its fallback \"{fallback_id}\" \
                         failed too: {}",
                    model.id, second.message
                ),
            )),
            other => other,
        })
}

/// Strips one leading `<think>...</think>` block from `content` (when
/// `enabled`) BEFORE the rest of the output pipeline (fences, parsing,
/// schema, trim/`max_lines`) runs, and logs the removed length — never its
/// content. Split out of `execute_business_command` only to keep it under
/// the crate's line-count lint (same convention as `build_messages` above).
fn strip_reasoning_and_log(content: &str, enabled: bool, logger: crate::log::Logger) -> String {
    let (stripped, stripped_chars) = crate::output::strip_reasoning(content, enabled);
    if stripped_chars > 0 {
        logger.info(&format!(
            "stripped {stripped_chars} characters of reasoning"
        ));
    }
    stripped.to_string()
}

/// Builds the full `messages` array for `spec`: `[system?] +
/// examples×[user, assistant] + [user: prompt]`, in file order. Split out
/// of `execute_business_command` only to keep it under the crate's
/// line-count lint — no behavior is different from what used to be
/// inlined there (same convention as
/// `backend::handle_non_streamed_response`).
///
/// With neither `system` nor `examples` declared, the result is exactly
/// today's single-element array (pinned by
/// `backend::tests::build_chat_request_without_system_or_examples_is_byte_identical_to_the_plain_body`).
fn build_messages(
    spec: &crate::command::CommandSpec,
    prompt: &str,
    args: &std::collections::BTreeMap<String, String>,
    env: &dyn Fn(&str) -> Option<String>,
    schemas: &std::collections::BTreeMap<String, String>,
) -> crate::Result<Vec<crate::backend::Message>> {
    let mut messages = Vec::with_capacity(1 + spec.examples.len() * 2 + 1);
    if let Some(system) = &spec.system {
        let rendered = crate::prompt::render(system, "", args, env, schemas)?;
        messages.push(crate::backend::Message {
            role: "system".to_string(),
            content: rendered,
        });
    }
    for example in &spec.examples {
        let user = crate::prompt::render(&example.user, "", args, env, schemas)?;
        let assistant = crate::prompt::render(&example.assistant, "", args, env, schemas)?;
        messages.push(crate::backend::Message {
            role: "user".to_string(),
            content: user,
        });
        messages.push(crate::backend::Message {
            role: "assistant".to_string(),
            content: assistant,
        });
    }
    messages.push(crate::backend::Message::user(prompt.to_string()));
    Ok(messages)
}

/// Executes the pipeline of an already-resolved BUSINESS command (`spec`),
/// with the loaded configuration (`config`, necessarily `Ok` at this point:
/// `run` has propagated any load error before reaching this function) and
/// the `ArgMatches` of the selected leaf command.
///
/// INVARIANT — preserved IDENTICALLY: nothing
/// that is knowable without the input must be checked after reading the
/// input. An argument referenced by the prompt and an environment variable
/// referenced by the prompt are both knowable even before knowing what
/// `{{ input }}` is worth: `prompt::preflight` therefore checks it BEFORE
/// `input::resolve`, which is the only step of this pipeline liable to
/// consume a non-replayable input (a pipe, a one-shot command).
/// Without this order, `git diff | npu ...` would
/// read and discard the whole diff before failing on a missing optional
/// argument or an undefined environment variable — a silent loss, and on a
/// non-replayable stream an irreversible one, of the work already produced
/// upstream.
#[allow(clippy::too_many_arguments)]
// env, read_input and stdout_is_terminal are the
// injected outside world; splitting them into a struct would only move the
// count, not reduce it.
pub(crate) fn execute_business_command(
    spec: &crate::command::CommandSpec,
    config: &crate::config::Config,
    leaf_matches: &clap::ArgMatches,
    logger: crate::log::Logger,
    env: &dyn Fn(&str) -> Option<String>,
    read_input: &dyn Fn(
        &crate::command::InputMode,
        Option<&std::path::Path>,
    ) -> crate::Result<String>,
    stdout_is_terminal: bool,
) -> crate::Result<()> {
    use crate::error::InFile;

    let (model, backend) = config.resolve(&spec.model)?;
    logger.info(&format!(
        "command \"{}\" -> model \"{}\" (backend \"{}\", operation \"{}\") from {}",
        spec.path.join("/"),
        model.id,
        backend.id,
        model.operation,
        spec.file.display()
    ));

    let args = crate::cli::collect_arg_values(spec, leaf_matches);
    // `env` is injected by the caller (`std::env::var(name).ok()` in
    // production): `Err` (missing variable, or invalid UTF-8) and a variable
    // defined but empty both flow through it exactly as `std::env::var`
    // would produce them, which is the semantics
    // `prompt::render`/`prompt::preflight` expect (contract rule 5: presence
    // checked at render time — and now at preflight time —, not at load
    // time). Exercised by `prompt::render`'s tests
    // (`render_env_var_defined_but_empty_is_not_an_error`) via this same
    // shape of closure.

    // Schemas are configuration, knowable without the input: read here,
    // before `input::resolve`, for the same reason as `preflight` (see
    // this function's doc). Only the invoked command's schemas are read.
    let output_schema = match (&spec.output.format, &spec.output.schema) {
        (crate::output::Format::Json, Some(path)) => {
            Some(crate::output::read_schema(path, &spec.file)?)
        }
        _ => None,
    };
    let schemas = spec
        .schemas
        .iter()
        .map(|(id, path)| {
            let document = crate::output::read_schema(path, &spec.file)?;
            Ok((id.clone(), document.to_string()))
        })
        .collect::<crate::Result<std::collections::BTreeMap<_, _>>>()?;

    crate::prompt::preflight(&spec.prompt, &args, env, &schemas)?;
    // `system` and every `[[examples]]` turn are templated exactly like the
    // body (minus `{{ input }}`, rejected at load time): their own
    // environment variables must be resolvable before the input is read,
    // same invariant as the body's own preflight above.
    if let Some(system) = &spec.system {
        crate::prompt::preflight(system, &args, env, &schemas)?;
    }
    for example in &spec.examples {
        crate::prompt::preflight(&example.user, &args, env, &schemas)?;
        crate::prompt::preflight(&example.assistant, &args, env, &schemas)?;
    }

    // Headers are resolved at preflight too, for the primary backend AND
    // (when declared) the fallback's: the fallback fires after the input
    // has already been consumed, so a missing environment variable there
    // must be caught here rather than at call time, same invariant as the
    // prompt's own preflight above. The resolved values are discarded here
    // (only the possibility of resolving them matters): `chat_with_fallback`
    // resolves them again, once it knows which backend it is actually
    // calling.
    crate::config::resolve_headers(backend, env)?;
    if let Some(fallback_id) = &model.fallback {
        let (_, fallback_backend) = config.resolve(fallback_id)?;
        crate::config::resolve_headers(fallback_backend, env)?;
    }

    // The `FILE` argument is only declared (cf. `build_clap_node`) for the
    // modes that accept a file: reproducing the same condition here avoids
    // calling `get_one` on an absent id (panic) and keeps the two spots in
    // sync if a later phase changes one without the other.
    let file_arg = match spec.input {
        crate::command::InputMode::File | crate::command::InputMode::StdinOrFile => leaf_matches
            .get_one::<String>("FILE")
            .map(std::path::Path::new),
        crate::command::InputMode::Stdin => None,
    };
    let input_text = read_input(&spec.input, file_arg)?;
    logger.info(&format!(
        "input: {} characters read from {}",
        input_text.chars().count(),
        match file_arg {
            Some(path) => path.display().to_string(),
            None => "stdin".to_string(),
        }
    ));

    let prompt = crate::prompt::render(&spec.prompt, &input_text, &args, env, &schemas)?;
    logger.info(&format!(
        "prompt rendered: {} characters",
        prompt.chars().count()
    ));

    // Builds the full message list: [system?] + examples×[user, assistant]
    // + [user: the rendered body]. With neither `system` nor `examples`
    // declared, this is exactly today's single-element array — pinned by
    // `backend::tests::build_chat_request_without_generation_options` and
    // the e2e test asserting the exact request bytes.
    let messages = build_messages(spec, &prompt, &args, env, &schemas)?;

    // The backend's raw response is never
    // written as-is to stdout. `output::finalize` applies the declared
    // output contract (`spec.output` — format, schema, max_lines) and
    // returns either the exact text to write, or an `Error::Output` (exit
    // code 4: the CONFIGURATION is valid, it is the model's response that
    // does not respect the declared contract — cf. `output.rs`'s module
    // doc). No reformulation or retry here: an invalid output is an
    // execution failure, not something to recover from.
    // Streamed only where nothing can reject the answer after it is shown
    // — free text, no `max_lines` — and only to a terminal: a pipe keeps
    // receiving the answer in one piece, byte for byte as before.
    // `strip_reasoning` disables streaming even on a terminal — printing
    // tokens as they arrive would show the reasoning block before it can be
    // stripped, defeating the whole point. The answer then arrives in one
    // piece, exactly as for a JSON contract.
    let streaming = stdout_is_terminal
        && spec.output.format == crate::output::Format::Text
        && spec.output.max_lines.is_none()
        && !spec.output.strip_reasoning;
    let started = std::cell::Cell::new(false);
    let print_token = |answered_by: &str, token: &str| {
        // The answer is trimmed like `finalize` trims it: nothing is shown
        // until the first non-blank token, which opens the frame.
        let token = if started.get() {
            token
        } else {
            let token = token.trim_start();
            if token.is_empty() {
                return;
            }
            anstream::print!("{}", answer_header(answered_by));
            started.set(true);
            token
        };
        anstream::print!("{token}");
        let _ = std::io::Write::flush(&mut anstream::stdout());
    };
    let ask = Ask {
        messages: &messages,
        schema: output_schema.as_ref(),
        stream: streaming.then_some(&print_token as TokenSink<'_>),
        command_generation: spec.generation.as_ref(),
    };
    let (answer, answered_by) = chat_with_fallback(config, model, backend, &ask, env, logger)?;

    // Truncation is a defect of the ANSWER, not of the prompt: it must never
    // trigger the fallback (that retry exists for a prompt too long, cf.
    // `chat_with_fallback`'s doc), and it must be checked before anything is
    // written to stdout on a pipe. On a terminal in streaming mode, tokens
    // already reached the screen through `print_token` — that is accepted
    // (cf. this crate's CLAUDE.md); the exit code is still 4 and
    // stdout (the byte stream a calling agent reads) never receives the
    // framed/closing output below.
    if answer.finish_reason.as_deref() == Some("length") && !spec.output.allow_truncated {
        let max_tokens = model.generation.max_tokens.map_or_else(
            || "server default".to_string(),
            |max_tokens| max_tokens.to_string(),
        );
        return Err(crate::Error::output(format!(
            "model \"{}\" answered with finish_reason \"length\" (truncated at max_tokens = \
             {max_tokens}); set [output].allow_truncated = true to accept a truncated answer",
            model.id
        )))
        .in_file(&spec.file);
    }

    let raw_output = strip_reasoning_and_log(&answer.content, spec.output.strip_reasoning, logger);
    let output = crate::output::finalize(&spec.output, &raw_output, &spec.file)?;
    logger.info(&format!(
        "output contract honoured ({}): {} characters written to stdout",
        spec.output.format.as_str(),
        output.chars().count()
    ));

    if started.get() {
        // Already on screen, token by token: only the frame is closed.
        anstream::print!(
            "{}",
            if raw_output.ends_with('\n') {
                "\n"
            } else {
                "\n\n"
            }
        );
    } else if stdout_is_terminal {
        anstream::print!("{}", framed_answer(&output, &answered_by));
    } else {
        println!("{output}");
    }

    Ok(())
}

/// The answer as a terminal shows it: set apart from the command line by a
/// blank line and a header naming the model that actually answered (the
/// fallback, when it took over), and closed by a blank line. Only ever
/// written to a TERMINAL: a pipe or a file receives the answer alone, byte
/// for byte, so no program ever parses this frame.
pub(crate) fn framed_answer(output: &str, answered_by: &str) -> String {
    format!("{}{output}\n\n", answer_header(answered_by))
}

/// The opening of [`framed_answer`], alone: what a streamed answer prints
/// before its first token.
pub(crate) fn answer_header(answered_by: &str) -> String {
    format!(
        "\n{} {}\n",
        crate::style::paint(crate::style::ANSWER_MARK, "●"),
        crate::style::paint(crate::style::ANSWER_HEADER, answered_by)
    )
}

#[cfg(test)]
#[allow(clippy::expect_used)] // allowed in tests (cf. Cargo.toml [lints.clippy]).
mod tests {
    use super::*;
    use crate::{Error, config, log};

    #[test]
    fn a_framed_answer_keeps_the_answer_verbatim_and_names_who_answered() {
        let framed = framed_answer("line 1\nline 2", "qwen-gpu");
        assert!(framed.contains("\nline 1\nline 2\n"));
        assert!(framed.contains("qwen-gpu"));
        assert!(framed.starts_with('\n'));
    }

    /// Serves ONE chat completion with the given status and body, then
    /// closes. Same technique as `backend.rs`'s end-to-end test: no HTTP
    /// dependency, just `std::net`.
    fn stub_backend(
        status_line: &str,
        body: &'static str,
    ) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{BufRead, BufReader, Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind of the stub");
        let addr = listener.local_addr().expect("local address of the stub");
        let status_line = status_line.to_string();

        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accepting the connection");
            let mut reader = BufReader::new(stream.try_clone().expect("cloning the TCP stream"));
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
            let mut drained = vec![0u8; content_length];
            reader.read_exact(&mut drained).expect("reading the body");

            let response = format!(
                "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: \
                 {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("writing the stub response");
        });

        (format!("http://{addr}"), handle)
    }

    /// Same idiom as [`stub_backend`], but hands the raw request body it
    /// received back to the caller through the returned channel, so a test
    /// can assert on the exact `messages` array `npu` sent.
    fn stub_backend_capturing_body(
        body: &'static str,
    ) -> (
        String,
        std::thread::JoinHandle<()>,
        std::sync::mpsc::Receiver<Vec<u8>>,
    ) {
        use std::io::{BufRead, BufReader, Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind of the stub");
        let addr = listener.local_addr().expect("local address of the stub");
        let (tx, rx) = std::sync::mpsc::channel();

        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accepting the connection");
            let mut reader = BufReader::new(stream.try_clone().expect("cloning the TCP stream"));
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
            let mut captured = vec![0u8; content_length];
            reader.read_exact(&mut captured).expect("reading the body");
            tx.send(captured).expect("sending the captured body back");

            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: \
                 {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("writing the stub response");
        });

        (format!("http://{addr}"), handle, rx)
    }

    fn backend_at(id: &str, base_url: String) -> config::Backend {
        config::Backend {
            id: id.to_string(),
            base_url,
            kind: "openai-compatible".to_string(),
            operations: [(
                "chat".to_string(),
                config::Operation {
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
        }
    }

    fn model_on(id: &str, backend: &str, fallback: Option<&str>) -> config::Model {
        config::Model {
            id: id.to_string(),
            backend: backend.to_string(),
            operation: "chat".to_string(),
            model: format!("{id}-underlying"),
            fallback: fallback.map(ToString::to_string),
            generation: config::Generation::default(),
            source: std::path::PathBuf::new(),
        }
    }

    /// The reason this feature exists: an NPU graph rejecting an over-long
    /// prompt with a clean 400 must be recovered from on the fallback model,
    /// not surfaced as a failure.
    #[test]
    fn chat_with_fallback_retries_on_a_backend_failure_and_returns_the_fallback_answer() {
        let (primary_url, primary_server) = stub_backend(
            "400 Bad Request",
            r#"{"error":"Input length exceeds the maximum allowed length"}"#,
        );
        let (fallback_url, fallback_server) = stub_backend(
            "200 OK",
            r#"{"choices":[{"message":{"role":"assistant","content":"from the fallback"}}]}"#,
        );

        let mut config = config::Config::default();
        config
            .backends
            .insert("npu".to_string(), backend_at("npu", primary_url));
        config
            .backends
            .insert("gpu".to_string(), backend_at("gpu", fallback_url));
        config
            .models
            .insert("small".to_string(), model_on("small", "npu", Some("big")));
        config
            .models
            .insert("big".to_string(), model_on("big", "gpu", None));

        let (model, backend) = config.resolve("small").expect("the fixture must resolve");
        let (output, answered_by) = chat_with_fallback(
            &config,
            model,
            backend,
            &Ask {
                messages: &[crate::backend::Message::user("hello")],
                schema: None,
                stream: None,
                command_generation: None,
            },
            &|_: &str| None,
            log::Logger::new(log::Level::Error),
        )
        .expect("the fallback must answer");

        assert_eq!(output.content, "from the fallback");
        assert_eq!(answered_by, "big");
        primary_server.join().expect("primary stub thread");
        fallback_server.join().expect("fallback stub thread");
    }

    #[test]
    fn chat_with_fallback_failing_on_both_names_both_models() {
        let (primary_url, primary_server) =
            stub_backend("400 Bad Request", r#"{"error":"too long"}"#);
        let (fallback_url, fallback_server) =
            stub_backend("500 Server Error", r#"{"error":"boom"}"#);

        let mut config = config::Config::default();
        config
            .backends
            .insert("npu".to_string(), backend_at("npu", primary_url));
        config
            .backends
            .insert("gpu".to_string(), backend_at("gpu", fallback_url));
        config
            .models
            .insert("small".to_string(), model_on("small", "npu", Some("big")));
        config
            .models
            .insert("big".to_string(), model_on("big", "gpu", None));

        let (model, backend) = config.resolve("small").expect("the fixture must resolve");
        let err = chat_with_fallback(
            &config,
            model,
            backend,
            &Ask {
                messages: &[crate::backend::Message::user("hello")],
                schema: None,
                stream: None,
                command_generation: None,
            },
            &|_: &str| None,
            log::Logger::new(log::Level::Error),
        )
        .expect_err("both backends failing must fail");

        assert!(matches!(err, Error::Backend(_)));
        let message = err.to_string();
        assert!(message.contains("small"), "got: {message}");
        assert!(message.contains("big"), "got: {message}");
        primary_server.join().expect("primary stub thread");
        fallback_server.join().expect("fallback stub thread");
    }

    #[test]
    fn chat_without_fallback_surfaces_the_primary_failure_unchanged() {
        let (primary_url, primary_server) =
            stub_backend("400 Bad Request", r#"{"error":"too long"}"#);

        let mut config = config::Config::default();
        config
            .backends
            .insert("npu".to_string(), backend_at("npu", primary_url));
        config
            .models
            .insert("small".to_string(), model_on("small", "npu", None));

        let (model, backend) = config.resolve("small").expect("the fixture must resolve");
        let err = chat_with_fallback(
            &config,
            model,
            backend,
            &Ask {
                messages: &[crate::backend::Message::user("hello")],
                schema: None,
                stream: None,
                command_generation: None,
            },
            &|_: &str| None,
            log::Logger::new(log::Level::Error),
        )
        .expect_err("a backend failure without fallback must stay a failure");

        assert_eq!(err.exit_code(), 3);
        primary_server.join().expect("primary stub thread");
    }

    /// The preflight check (a missing environment variable the prompt
    /// references) must reject the command BEFORE `read_input` is ever
    /// called: `read_input` panicking here proves it, since a passing test
    /// means the panic never fired.
    #[test]
    #[allow(clippy::panic)] // the panic is the assertion: read_input must not run.
    fn a_missing_env_var_is_rejected_before_read_input_is_called() {
        let mut config = config::Config::default();
        config.backends.insert(
            "b".to_string(),
            backend_at("b", "http://127.0.0.1:9".to_string()),
        );
        config
            .models
            .insert("qwen-fast".to_string(), model_on("qwen-fast", "b", None));

        let specs = vec![crate::command::CommandSpec {
            path: vec!["x".to_string()],
            description: "desc x".to_string(),
            model: "qwen-fast".to_string(),
            input: crate::command::InputMode::Stdin,
            prompt: "{{ env.NPU_TEST_UNSET }} {{ input }}".to_string(),
            args: std::collections::BTreeMap::new(),
            output: crate::output::OutputSpec::default(),
            schemas: std::collections::BTreeMap::new(),
            system: None,
            examples: Vec::new(),
            generation: None,
            file: std::path::PathBuf::new(),
        }];
        let cli = crate::cli::build_cli(&specs);
        let matches = cli
            .try_get_matches_from(["npu", "x"])
            .expect("the command line must be accepted");
        let (_, leaf_matches) = crate::cli::selected_path(&matches);

        let env = |_: &str| None;
        let read_input = |_: &crate::command::InputMode, _: Option<&std::path::Path>| {
            panic!("read_input must not be called when preflight already fails")
        };

        let err = execute_business_command(
            &specs[0],
            &config,
            leaf_matches,
            log::Logger::new(log::Level::Error),
            &env,
            &read_input,
            false,
        )
        .expect_err("a prompt referencing an undefined environment variable must be rejected");

        assert!(matches!(err, Error::Config(_)));
        assert!(err.to_string().contains("NPU_TEST_UNSET"));
    }

    /// Same invariant as above, but for a backend `[headers]` value
    /// referencing an undefined environment variable: preflight must
    /// resolve headers before `input::resolve` runs, exactly like the
    /// prompt's own placeholders.
    #[test]
    #[allow(clippy::panic)] // the panic is the assertion: read_input must not run.
    fn a_missing_header_env_var_is_rejected_before_read_input_is_called() {
        let mut backend = backend_at("b", "http://127.0.0.1:9".to_string());
        backend.headers.insert(
            "Authorization".to_string(),
            "Bearer {{ env.NPU_TEST_UNSET_HEADER_VAR }}".to_string(),
        );
        let mut config = config::Config::default();
        config.backends.insert("b".to_string(), backend);
        config
            .models
            .insert("qwen-fast".to_string(), model_on("qwen-fast", "b", None));

        let specs = vec![crate::command::CommandSpec {
            path: vec!["x".to_string()],
            description: "desc x".to_string(),
            model: "qwen-fast".to_string(),
            input: crate::command::InputMode::Stdin,
            prompt: "{{ input }}".to_string(),
            args: std::collections::BTreeMap::new(),
            output: crate::output::OutputSpec::default(),
            schemas: std::collections::BTreeMap::new(),
            system: None,
            examples: Vec::new(),
            generation: None,
            file: std::path::PathBuf::new(),
        }];
        let cli = crate::cli::build_cli(&specs);
        let matches = cli
            .try_get_matches_from(["npu", "x"])
            .expect("the command line must be accepted");
        let (_, leaf_matches) = crate::cli::selected_path(&matches);

        let env = |_: &str| None;
        let read_input = |_: &crate::command::InputMode, _: Option<&std::path::Path>| {
            panic!("read_input must not be called when header preflight already fails")
        };

        let err = execute_business_command(
            &specs[0],
            &config,
            leaf_matches,
            log::Logger::new(log::Level::Error),
            &env,
            &read_input,
            false,
        )
        .expect_err("a header referencing an undefined environment variable must be rejected");

        assert!(matches!(err, Error::Config(_)));
        assert!(err.to_string().contains("NPU_TEST_UNSET_HEADER_VAR"));
    }

    /// Spec: `chat_with_fallback` streaming a couple of deltas (so `emitted`
    /// is set) and then failing must return `Err(Backend)` WITHOUT ever
    /// attempting the fallback: the fallback backend below is unreachable,
    /// so an attempt to contact it would fail differently and its id would
    /// show up in the message (cf. the "failed too" phrasing built by
    /// `chat_with_fallback` when the fallback IS attempted).
    #[test]
    fn chat_with_fallback_does_not_retry_once_a_token_has_been_emitted() {
        let (primary_url, primary_server) = stub_backend(
            "200 OK",
            "data: {\"choices\":[{\"delta\":{\"content\":\"Bon\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{\"content\":\"jour\"}}]}\n\n\
             data: {\"error\":{\"message\":\"boom mid-stream\"}}\n\n",
        );

        let mut config = config::Config::default();
        config
            .backends
            .insert("npu".to_string(), backend_at("npu", primary_url));
        // Deliberately unreachable: if `chat_with_fallback` attempted it
        // regardless of `emitted`, the failure message would differ from a
        // plain primary-only failure (it would name "big" too).
        config.backends.insert(
            "gpu".to_string(),
            backend_at("gpu", "http://127.0.0.1:1".to_string()),
        );
        config
            .models
            .insert("small".to_string(), model_on("small", "npu", Some("big")));
        config
            .models
            .insert("big".to_string(), model_on("big", "gpu", None));

        let seen = std::cell::RefCell::new(Vec::new());
        let sink = |_: &str, token: &str| seen.borrow_mut().push(token.to_string());

        let (model, backend) = config.resolve("small").expect("the fixture must resolve");
        let err = chat_with_fallback(
            &config,
            model,
            backend,
            &Ask {
                messages: &[crate::backend::Message::user("hello")],
                schema: None,
                stream: Some(&sink),
                command_generation: None,
            },
            &|_: &str| None,
            log::Logger::new(log::Level::Error),
        )
        .expect_err("a mid-stream error after emitted tokens must stay a failure");

        assert!(matches!(err, Error::Backend(_)));
        assert_eq!(err.exit_code(), 3);
        assert_eq!(*seen.borrow(), ["Bon", "jour"]);
        let message = err.to_string();
        assert!(
            !message.contains("big") && !message.contains("failed too"),
            "the fallback must not have been attempted once a token was emitted, got: {message}"
        );
        primary_server.join().expect("primary stub thread");
    }

    /// A streamed answer whose stream ends with
    /// `finish_reason = "length"` must fail with `Error::Output` (exit 4)
    /// even when tokens already reached a terminal (`stdout_is_terminal =
    /// true`), naming the model id; `allow_truncated` is left at its
    /// default (`false`) by `OutputSpec::default()`.
    #[test]
    fn a_truncated_streamed_answer_fails_with_output_error_even_on_a_terminal() {
        let (url, server) = stub_backend(
            "200 OK",
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
             data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n\
             data: [DONE]\n\n",
        );

        let mut config = config::Config::default();
        config
            .backends
            .insert("b".to_string(), backend_at("b", url));
        config
            .models
            .insert("qwen-fast".to_string(), model_on("qwen-fast", "b", None));

        let specs = vec![crate::command::CommandSpec {
            path: vec!["x".to_string()],
            description: "desc x".to_string(),
            model: "qwen-fast".to_string(),
            input: crate::command::InputMode::Stdin,
            prompt: "{{ input }}".to_string(),
            args: std::collections::BTreeMap::new(),
            output: crate::output::OutputSpec::default(),
            schemas: std::collections::BTreeMap::new(),
            system: None,
            examples: Vec::new(),
            generation: None,
            file: std::path::PathBuf::new(),
        }];
        let cli = crate::cli::build_cli(&specs);
        let matches = cli
            .try_get_matches_from(["npu", "x"])
            .expect("the command line must be accepted");
        let (_, leaf_matches) = crate::cli::selected_path(&matches);

        let env = |_: &str| None;
        let read_input =
            |_: &crate::command::InputMode, _: Option<&std::path::Path>| Ok("hi".to_string());

        let err = execute_business_command(
            &specs[0],
            &config,
            leaf_matches,
            log::Logger::new(log::Level::Error),
            &env,
            &read_input,
            true,
        )
        .expect_err("a truncated answer must fail even when streamed to a terminal");

        assert!(matches!(err, Error::Output(_)));
        assert_eq!(err.exit_code(), 4);
        assert!(
            err.to_string().contains("qwen-fast"),
            "the message must name the model, got: {err}"
        );
        server.join().expect("stub server thread");
    }

    /// A command declaring `system` and `[[examples]]` must send them,
    /// in order, ahead of the rendered body as the final `user` message.
    #[test]
    fn system_and_examples_are_sent_in_order_ahead_of_the_body() {
        let (url, server, rx) = stub_backend_capturing_body(
            r#"{"choices":[{"message":{"role":"assistant","content":"ok"}}]}"#,
        );

        let mut config = config::Config::default();
        config
            .backends
            .insert("b".to_string(), backend_at("b", url));
        config
            .models
            .insert("qwen-fast".to_string(), model_on("qwen-fast", "b", None));

        let specs = vec![crate::command::CommandSpec {
            path: vec!["x".to_string()],
            description: "desc x".to_string(),
            model: "qwen-fast".to_string(),
            input: crate::command::InputMode::Stdin,
            prompt: "{{ input }}".to_string(),
            args: std::collections::BTreeMap::new(),
            output: crate::output::OutputSpec::default(),
            schemas: std::collections::BTreeMap::new(),
            system: Some("be terse".to_string()),
            examples: vec![crate::command::Example {
                user: "ticket: printer on fire".to_string(),
                assistant: r#"{"category":"hardware"}"#.to_string(),
            }],
            generation: None,
            file: std::path::PathBuf::new(),
        }];
        let cli = crate::cli::build_cli(&specs);
        let matches = cli
            .try_get_matches_from(["npu", "x"])
            .expect("the command line must be accepted");
        let (_, leaf_matches) = crate::cli::selected_path(&matches);

        let env = |_: &str| None;
        let read_input = |_: &crate::command::InputMode, _: Option<&std::path::Path>| {
            Ok("body text".to_string())
        };

        execute_business_command(
            &specs[0],
            &config,
            leaf_matches,
            log::Logger::new(log::Level::Error),
            &env,
            &read_input,
            false,
        )
        .expect("must succeed");

        let captured = rx.recv().expect("the stub must report the captured body");
        let body: serde_json::Value =
            serde_json::from_slice(&captured).expect("the body must be JSON");
        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "be terse");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "ticket: printer on fire");
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[2]["content"], r#"{"category":"hardware"}"#);
        assert_eq!(messages[3]["role"], "user");
        assert_eq!(messages[3]["content"], "body text");

        server.join().expect("stub server thread");
    }

    /// An undefined environment variable referenced only by
    /// `system` must be rejected BEFORE `read_input` is ever called, same
    /// invariant as the prompt's own placeholders.
    #[test]
    #[allow(clippy::panic)] // the panic is the assertion: read_input must not run.
    fn a_missing_system_env_var_is_rejected_before_read_input_is_called() {
        let mut config = config::Config::default();
        config.backends.insert(
            "b".to_string(),
            backend_at("b", "http://127.0.0.1:9".to_string()),
        );
        config
            .models
            .insert("qwen-fast".to_string(), model_on("qwen-fast", "b", None));

        let specs = vec![crate::command::CommandSpec {
            path: vec!["x".to_string()],
            description: "desc x".to_string(),
            model: "qwen-fast".to_string(),
            input: crate::command::InputMode::Stdin,
            prompt: "{{ input }}".to_string(),
            args: std::collections::BTreeMap::new(),
            output: crate::output::OutputSpec::default(),
            schemas: std::collections::BTreeMap::new(),
            system: Some("{{ env.NPU_TEST_UNSET_SYSTEM_VAR }}".to_string()),
            examples: Vec::new(),
            generation: None,
            file: std::path::PathBuf::new(),
        }];
        let cli = crate::cli::build_cli(&specs);
        let matches = cli
            .try_get_matches_from(["npu", "x"])
            .expect("the command line must be accepted");
        let (_, leaf_matches) = crate::cli::selected_path(&matches);

        let env = |_: &str| None;
        let read_input = |_: &crate::command::InputMode, _: Option<&std::path::Path>| {
            panic!("read_input must not be called when system preflight already fails")
        };

        let err = execute_business_command(
            &specs[0],
            &config,
            leaf_matches,
            log::Logger::new(log::Level::Error),
            &env,
            &read_input,
            false,
        )
        .expect_err("a system referencing an undefined environment variable must be rejected");

        assert!(matches!(err, Error::Config(_)));
        assert!(err.to_string().contains("NPU_TEST_UNSET_SYSTEM_VAR"));
    }

    /// `strip_reasoning = true` must disable streaming even when
    /// `stdout_is_terminal = true` — the condition that would otherwise
    /// enable it (cf. the `streaming` computation in
    /// `execute_business_command`). Asserted on the REQUEST the stub
    /// server actually received: no `"stream": true` in the body, which is
    /// the discriminator streaming vs. non-streaming turns on.
    #[test]
    fn strip_reasoning_disables_streaming_even_on_a_terminal() {
        let (url, server, rx) = stub_backend_capturing_body(
            r#"{"choices":[{"message":{"role":"assistant","content":"<think>t</think>final"}}]}"#,
        );

        let mut config = config::Config::default();
        config
            .backends
            .insert("b".to_string(), backend_at("b", url));
        config
            .models
            .insert("qwen-fast".to_string(), model_on("qwen-fast", "b", None));

        let output = crate::output::OutputSpec {
            strip_reasoning: true,
            ..crate::output::OutputSpec::default()
        };

        let specs = vec![crate::command::CommandSpec {
            path: vec!["x".to_string()],
            description: "desc x".to_string(),
            model: "qwen-fast".to_string(),
            input: crate::command::InputMode::Stdin,
            prompt: "{{ input }}".to_string(),
            args: std::collections::BTreeMap::new(),
            output,
            schemas: std::collections::BTreeMap::new(),
            system: None,
            examples: Vec::new(),
            generation: None,
            file: std::path::PathBuf::new(),
        }];
        let cli = crate::cli::build_cli(&specs);
        let matches = cli
            .try_get_matches_from(["npu", "x"])
            .expect("the command line must be accepted");
        let (_, leaf_matches) = crate::cli::selected_path(&matches);

        let env = |_: &str| None;
        let read_input =
            |_: &crate::command::InputMode, _: Option<&std::path::Path>| Ok("hi".to_string());

        execute_business_command(
            &specs[0],
            &config,
            leaf_matches,
            log::Logger::new(log::Level::Error),
            &env,
            &read_input,
            // The condition that would otherwise enable streaming.
            true,
        )
        .expect("must succeed");

        let captured = rx.recv().expect("the stub must report the captured body");
        let body: serde_json::Value =
            serde_json::from_slice(&captured).expect("the body must be JSON");
        assert!(
            body.get("stream").is_none() || body["stream"] == false,
            "strip_reasoning must disable streaming, got body: {body}"
        );

        server.join().expect("stub server thread");
    }
}

//! Business execution: resolving a command's model, rendering its prompt,
//! calling the backend (with the single-hop fallback retry) and writing the
//! answer to stdout under the output contract.

/// Where a streamed answer's tokens go: `(model that answers, token)`.
type TokenSink<'a> = &'a dyn Fn(&str, &str);

/// What [`chat_with_fallback`] asks of the model and, failing it, of its
/// fallback.
#[derive(Clone, Copy)]
pub(crate) struct Ask<'a> {
    pub(crate) prompt: &'a str,
    pub(crate) schema: Option<&'a serde_json::Value>,
    pub(crate) stream: Option<TokenSink<'a>>,
}

// `&dyn Fn` cannot derive `Debug`: the closure has nothing to print.
impl std::fmt::Debug for Ask<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ask")
            .field("prompt", &self.prompt)
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
    logger: crate::log::Logger,
) -> crate::Result<(String, String)> {
    let Ask {
        prompt,
        schema,
        stream,
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
        crate::runtime::resolve_base_url(backend, &crate::runtime::docker::runner).and_then(
            |base_url| {
                let request = crate::backend::Request {
                    prompt,
                    schema,
                    on_token,
                };
                crate::backend::chat(backend, model, &base_url, &request, logger)
            },
        )
    };

    let primary = match call(backend, model) {
        Ok(output) => return Ok((output, model.id.clone())),
        Err(crate::Error::Backend(message)) if !emitted.get() => message,
        Err(other) => return Err(other),
    };

    let Some(fallback_id) = model.fallback.as_deref() else {
        return Err(crate::Error::Backend(primary));
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
            crate::Error::Backend(second) => crate::Error::Backend(format!(
                "model \"{}\" failed ({primary}), and its fallback \"{fallback_id}\" failed too: \
             {second}",
                model.id
            )),
            other => other,
        })
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
pub(crate) fn execute_business_command(
    spec: &crate::command::CommandSpec,
    config: &crate::config::Config,
    leaf_matches: &clap::ArgMatches,
    logger: crate::log::Logger,
) -> crate::Result<()> {
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
    // `std::env::var` returns `Err` both for a missing variable and for a
    // variable containing invalid UTF-8; `.ok()` reduces both cases to
    // `None`, exactly the semantics expected by
    // `prompt::render`/`prompt::preflight` (contract rule 5: presence
    // checked at render time — and now at preflight time —, not at load
    // time). A variable that is defined but empty stays `Ok(String::new())`
    // on the `std::env::var` side (it is neither missing nor invalid), so
    // `Some(String::new())` here, never `None` — guaranteed by `std`,
    // exercised by `prompt::render`'s tests
    // (`render_env_var_defined_but_empty_is_not_an_error`) via this same
    // injected closure. This point cannot be checked directly by a test
    // HERE without mutating the real environment, forbidden by
    // `unsafe_code = "forbid"` in edition 2024 (the same constraint
    // documented on `prompt::render`): the closure itself therefore remains
    // the smallest untestable unit, the rest of the semantics is validated
    // on the `prompt.rs` side.
    let env = |name: &str| std::env::var(name).ok();

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

    crate::prompt::preflight(&spec.prompt, &args, &env, &schemas)?;

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
    let input_text = crate::input::resolve(&spec.input, file_arg)?;
    logger.info(&format!(
        "input: {} characters read from {}",
        input_text.chars().count(),
        match file_arg {
            Some(path) => path.display().to_string(),
            None => "stdin".to_string(),
        }
    ));

    let prompt = crate::prompt::render(&spec.prompt, &input_text, &args, &env, &schemas)?;
    logger.info(&format!(
        "prompt rendered: {} characters",
        prompt.chars().count()
    ));

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
    let streaming = std::io::IsTerminal::is_terminal(&std::io::stdout())
        && spec.output.format == crate::output::Format::Text
        && spec.output.max_lines.is_none();
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
        prompt: &prompt,
        schema: output_schema.as_ref(),
        stream: streaming.then_some(&print_token as TokenSink<'_>),
    };
    let (raw_output, answered_by) = chat_with_fallback(config, model, backend, &ask, logger)?;
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
    } else if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
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
                prompt: "hello",
                schema: None,
                stream: None,
            },
            log::Logger::new(log::Level::Error),
        )
        .expect("the fallback must answer");

        assert_eq!(output, "from the fallback");
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
                prompt: "hello",
                schema: None,
                stream: None,
            },
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
                prompt: "hello",
                schema: None,
                stream: None,
            },
            log::Logger::new(log::Level::Error),
        )
        .expect_err("a backend failure without fallback must stay a failure");

        assert_eq!(err.exit_code(), 3);
        primary_server.join().expect("primary stub thread");
    }
}

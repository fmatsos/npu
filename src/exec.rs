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
    /// Fail at once, instead of waiting, when a `max_concurrent = 1`
    /// backend is busy with another process's request (`--no-wait`).
    pub(crate) no_wait: bool,
    /// The bytes of a `binary` command's input, uploaded as-is.
    pub(crate) upload: Option<crate::backend::Upload<'a>>,
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
        no_wait,
        upload,
    } = *ask;
    let state = crate::runtime::state::StateEnv::from_vars(env);
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
        // Held until this attempt returns, stream included, and released
        // BEFORE the fallback's attempt: a fallback on the same backend
        // must not wait for its own primary's lock.
        let slot = crate::runtime::state::acquire_request_slot(&state, backend, !no_wait, &|| {
            spinner.set_message(&format!("waiting for backend \"{}\" (busy)", backend.id));
        })?;
        if slot.is_some() {
            spinner.set_message(&format!("waiting for model \"{}\"", model.id));
        }
        crate::runtime::resolve_base_url(backend, &crate::runtime::docker::runner).and_then(
            |base_url| {
                let request = crate::backend::Request {
                    messages,
                    schema,
                    on_token,
                    headers: &headers,
                    generation: &generation,
                    upload,
                };
                crate::backend::chat(backend, model, &base_url, &request, logger)
            },
        )
    };

    let primary = match call(backend, model) {
        Ok(output) => return Ok((output, model.id.clone())),
        Err(crate::Error::Backend(err)) if !emitted.get() => err,
        Err(other) => return Err(other),
    };

    // Without a fallback the primary's error is returned as is, URL and
    // status included.
    let Some(fallback_id) = model.fallback.as_deref() else {
        return Err(crate::Error::Backend(primary));
    };
    let primary = primary.message;

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
            // The fallback's own error, backend, URL and status kept; only
            // the message is widened to tell both failures.
            crate::Error::Backend(mut second) => {
                second.message = format!(
                    "model \"{}\" failed ({primary}), and its fallback \"{fallback_id}\" \
                     failed too: {}",
                    model.id, second.message
                );
                crate::Error::Backend(second)
            }
            other => other,
        })
}

/// The value every redacted header shows instead of its real value: a
/// `--dry-run` report is meant to be pasted into a bug report or an agent's
/// transcript, and a header value (an API key, a bearer token) must never
/// leak through it.
const REDACTED_HEADER_VALUE: &str = "<redacted>";

/// Whether a real (non-dry-run) invocation of `spec` would stream its
/// answer: only to a terminal, only for free text, only with no
/// `max_lines` to enforce after the fact, and never when `strip_reasoning`
/// would otherwise show the reasoning block before it can be stripped.
/// Shared by the real request path and `--dry-run`, so the reported
/// `"stream"` field never drifts from what `chat` would actually send.
fn would_stream(spec: &crate::command::CommandSpec, stdout_is_terminal: bool) -> bool {
    stdout_is_terminal
        && spec.output.format == crate::output::Format::Text
        && spec.output.max_lines.is_none()
        && !spec.output.strip_reasoning
        // A binary input only goes to a transcription, which answers in
        // one piece.
        && !matches!(spec.input, crate::command::InputMode::Binary)
}

/// `npu <command> --dry-run`'s whole job: build the exact request `chat`
/// would send to the PRIMARY model (never the fallback — a dry run shows
/// what would be tried first), through the same [`crate::backend::build_request`]
/// constructor `chat` itself uses, then print it as JSON and return without
/// ever reaching the network.
///
/// Two things a real call would do are deliberately skipped:
/// - the runtime is never resolved (`runtime::resolve_base_url`): a
///   `port = "auto"` backend's `base_url` is shown with its
///   `{{ backend.port }}` placeholder exactly as declared, since resolving
///   it means asking Docker, which a dry run must never do;
/// - header VALUES are redacted (`REDACTED_HEADER_VALUE`): only their names
///   are shown, same discipline as the "POST ..." info log line in
///   `backend::chat`.
#[allow(clippy::too_many_arguments)]
fn print_dry_run(
    model: &crate::config::Model,
    backend: &crate::config::Backend,
    messages: &[crate::backend::Message],
    upload: Option<crate::backend::Upload<'_>>,
    schema: Option<&serde_json::Value>,
    spec: &crate::command::CommandSpec,
    env: &dyn Fn(&str) -> Option<String>,
    stdout_is_terminal: bool,
) -> crate::Result<()> {
    let headers = crate::config::resolve_headers(backend, env)?;
    let generation = crate::config::Generation::merged(&model.generation, spec.generation.as_ref());
    let schema = schema.filter(|_| backend.structured_output);
    let request = crate::backend::Request {
        messages,
        schema,
        on_token: None,
        headers: &headers,
        generation: &generation,
        upload,
    };
    let prepared = crate::backend::build_request(backend, model, &backend.base_url, &request)?;

    let redacted_headers: std::collections::BTreeMap<&str, &str> = prepared
        .headers
        .keys()
        .map(|name| (name.as_str(), REDACTED_HEADER_VALUE))
        .collect();

    // `on_token` above is always `None`, so `build_request` never sets
    // `"stream"` on its own (`backend::chat` sets it afterwards, keyed on
    // whether it was actually given a token callback) — the dry run must
    // compute the same decision a real call would make and add it here, or
    // the report would silently omit `"stream": true` for every command
    // that would in fact stream.
    let mut body = prepared.body;
    if would_stream(spec, stdout_is_terminal)
        && let serde_json::Value::Object(map) = &mut body
    {
        map.insert("stream".to_string(), serde_json::Value::Bool(true));
    }

    let report = serde_json::json!({
        "url": prepared.url,
        "headers": redacted_headers,
        "body": body,
    });
    println!("{report}");
    Ok(())
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
    partials: &std::collections::BTreeMap<String, String>,
) -> crate::Result<Vec<crate::backend::Message>> {
    let mut messages = Vec::with_capacity(1 + spec.examples.len() * 2 + 1);
    if let Some(system) = &spec.system {
        let rendered = crate::prompt::render(system, "", args, env, schemas, partials)?;
        messages.push(crate::backend::Message {
            role: "system".to_string(),
            content: rendered,
        });
    }
    for example in &spec.examples {
        let user = crate::prompt::render(&example.user, "", args, env, schemas, partials)?;
        let assistant =
            crate::prompt::render(&example.assistant, "", args, env, schemas, partials)?;
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

/// Resolves headers at preflight time, for the primary backend AND (when
/// declared) the fallback's: the fallback fires after the input has already
/// been consumed, so a missing environment variable there must be caught
/// here rather than at call time, same invariant as the prompt's own
/// preflight. The resolved values are discarded here (only the possibility
/// of resolving them matters): `chat_with_fallback` resolves them again,
/// once it knows which backend it is actually calling. Split out of
/// `execute_business_command` only to keep it under the crate's line-count
/// lint, same convention as `build_messages` above.
fn preflight_headers(
    config: &crate::config::Config,
    model: &crate::config::Model,
    backend: &crate::config::Backend,
    env: &dyn Fn(&str) -> Option<String>,
) -> crate::Result<()> {
    crate::config::resolve_headers(backend, env)?;
    if let Some(fallback_id) = &model.fallback {
        let (_, fallback_backend) = config.resolve(fallback_id)?;
        crate::config::resolve_headers(fallback_backend, env)?;
    }
    Ok(())
}

/// Fails with `Error::Output` when the answer was cut at `max_tokens` and
/// the command does not accept a truncated answer.
fn reject_truncation(
    answer: &crate::backend::ChatAnswer,
    answered_by: &str,
    spec: &crate::command::CommandSpec,
    config: &crate::config::Config,
    model: &crate::config::Model,
) -> crate::Result<()> {
    use crate::error::InFile;

    if answer.finish_reason.as_deref() == Some("length") && !spec.output.allow_truncated {
        // Named after the model that ANSWERED — the fallback, when it took
        // over — with the limit in effect for it (its own `[generation]`
        // under the command's override).
        let answering = if answered_by == model.id {
            model
        } else {
            config.resolve(answered_by)?.0
        };
        let generation =
            crate::config::Generation::merged(&answering.generation, spec.generation.as_ref());
        let max_tokens = generation.max_tokens.map_or_else(
            || "server default".to_string(),
            |max_tokens| max_tokens.to_string(),
        );
        return Err(crate::Error::output(format!(
            "model \"{}\" answered with finish_reason \"length\" (truncated at max_tokens = \
             {max_tokens}); set [output].allow_truncated = true to accept a truncated answer",
            answering.id
        )))
        .in_file(&spec.file);
    }

    Ok(())
}

const BINARY_ONLY_FOR_TRANSCRIPTIONS: &str =
    "[input] mode = \"binary\" (only a transcriptions operation takes bytes)";

/// Rejects a command whose keys the protocol of `model`'s operation cannot
/// honour, naming the command file and the model. Checked when the command
/// runs, not when it is loaded: `--model` can swap in a model of another
/// protocol. `npu doctor` runs the same check on every command.
pub(crate) fn check_protocol(
    spec: &crate::command::CommandSpec,
    model: &crate::config::Model,
    backend: &crate::config::Backend,
) -> crate::Result<()> {
    let Some(operation) = backend.operations.get(&model.operation) else {
        // Reported, with the available operations, when the request is built.
        return Ok(());
    };
    let binary = matches!(spec.input, crate::command::InputMode::Binary);
    let offending = match operation.protocol {
        crate::config::Protocol::Chat => binary.then_some(BINARY_ONLY_FOR_TRANSCRIPTIONS),
        crate::config::Protocol::Transcriptions => [
            (
                !binary,
                "an [input] mode other than \"binary\" (the operation uploads bytes)",
            ),
            (
                spec.output.format != crate::output::Format::Text,
                "format = \"json\" (the answer is text)",
            ),
            (spec.system.is_some(), "system"),
            (!spec.examples.is_empty(), "[[examples]]"),
            (spec.generation.is_some(), "[generation]"),
            (spec.output.strip_reasoning, "[output].strip_reasoning"),
            (spec.output.allow_truncated, "[output].allow_truncated"),
        ]
        .into_iter()
        .find_map(|(declared, key)| declared.then_some(key)),
        crate::config::Protocol::Embeddings => [
            (binary, BINARY_ONLY_FOR_TRANSCRIPTIONS),
            (
                spec.output.format != crate::output::Format::Json,
                "format = \"text\" (the answer is a vector: declare format = \"json\")",
            ),
            (spec.system.is_some(), "system"),
            (!spec.examples.is_empty(), "[[examples]]"),
            (spec.generation.is_some(), "[generation]"),
            (spec.output.strip_reasoning, "[output].strip_reasoning"),
            (spec.output.allow_truncated, "[output].allow_truncated"),
        ]
        .into_iter()
        .find_map(|(declared, key)| declared.then_some(key)),
    };
    match offending {
        None => Ok(()),
        Some(key) => Err(crate::Error::Config(crate::error::ConfigError::in_file(
            &spec.file,
            Some(&model.id),
            format!(
                "command \"{}\" runs model \"{}\", whose operation \"{}\" speaks {}: \
                 {key} does not apply to it",
                spec.path.join("/"),
                model.id,
                model.operation,
                operation.protocol.as_str()
            ),
        ))),
    }
}

struct PreparedCommand<'a> {
    model: &'a crate::config::Model,
    backend: &'a crate::config::Backend,
    messages: Vec<crate::backend::Message>,
    output_schema: Option<serde_json::Value>,
    /// `--no-wait`: set by the CLI after preparing, `false` everywhere
    /// else.
    no_wait: bool,
    /// A `binary` command's input: its bytes and the name they carry.
    upload: Option<(Vec<u8>, String)>,
}

impl PreparedCommand<'_> {
    fn upload(&self) -> Option<crate::backend::Upload<'_>> {
        self.upload
            .as_ref()
            .map(|(data, name)| crate::backend::Upload { data, name })
    }
}

#[allow(clippy::too_many_arguments)]
fn prepare_command<'a>(
    spec: &crate::command::CommandSpec,
    config: &'a crate::config::Config,
    model_id: &str,
    args: &std::collections::BTreeMap<String, String>,
    env: &dyn Fn(&str) -> Option<String>,
    read_input: impl FnOnce() -> crate::Result<crate::input::Input>,
    logger: crate::log::Logger,
    file_args_are_content: bool,
) -> crate::Result<PreparedCommand<'a>> {
    let (model, backend) = config.resolve(model_id)?;
    check_protocol(spec, model, backend)?;
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
    let partials = spec
        .partials
        .iter()
        .map(|(id, path)| Ok((id.clone(), crate::prompt::read_partial(path, &spec.file)?)))
        .collect::<crate::Result<std::collections::BTreeMap<_, _>>>()?;
    crate::prompt::preflight(&spec.prompt, args, env, &schemas, &partials)?;
    if let Some(system) = &spec.system {
        crate::prompt::preflight(system, args, env, &schemas, &partials)?;
    }
    for example in &spec.examples {
        crate::prompt::preflight(&example.user, args, env, &schemas, &partials)?;
        crate::prompt::preflight(&example.assistant, args, env, &schemas, &partials)?;
    }
    preflight_headers(config, model, backend, env)?;
    let mut resolved_args = args.clone();
    for (name, declaration) in &spec.args {
        if !file_args_are_content
            && matches!(declaration.kind, crate::command::ArgType::File)
            && let Some(path) = args.get(name)
        {
            let content = crate::input::resolve(
                &crate::command::InputMode::File,
                Some(std::path::Path::new(path)),
            )?;
            resolved_args.insert(name.clone(), content);
        }
    }
    let (input, upload) = match read_input()? {
        crate::input::Input::Text(text) => {
            logger.info(&format!("input: {} characters read", text.chars().count()));
            (text, None)
        }
        crate::input::Input::Bytes { data, name } => {
            logger.info(&format!("input: {} bytes read from {name}", data.len()));
            (String::new(), Some((data, name)))
        }
    };
    let prompt = crate::prompt::render(
        &spec.prompt,
        &input,
        &resolved_args,
        env,
        &schemas,
        &partials,
    )?;
    let messages = build_messages(spec, &prompt, &resolved_args, env, &schemas, &partials)?;
    Ok(PreparedCommand {
        model,
        backend,
        messages,
        output_schema,
        no_wait: false,
        upload,
    })
}

#[allow(clippy::too_many_arguments)]
fn run_prepared(
    spec: &crate::command::CommandSpec,
    config: &crate::config::Config,
    prepared: &PreparedCommand<'_>,
    env: &dyn Fn(&str) -> Option<String>,
    logger: crate::log::Logger,
    stream: Option<TokenSink<'_>>,
    record: &mut crate::stats::Record,
) -> crate::Result<(String, String, String)> {
    let ask = Ask {
        messages: &prepared.messages,
        schema: prepared.output_schema.as_ref(),
        stream,
        command_generation: spec.generation.as_ref(),
        no_wait: prepared.no_wait,
        upload: prepared.upload(),
    };
    record.backend = Some(prepared.backend.id.clone());
    let started = std::time::Instant::now();
    let answered = chat_with_fallback(config, prepared.model, prepared.backend, &ask, env, logger);
    record.duration_ms = Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
    let (answer, answered_by) = answered?;
    let fallback_used = answered_by != prepared.model.id;
    if fallback_used {
        record.backend = config
            .resolve(&answered_by)
            .ok()
            .map(|(_, backend)| backend.id.clone());
    }
    record.model_answered = Some(answered_by.clone());
    record.fallback_used = Some(fallback_used);
    record.finish_reason.clone_from(&answer.finish_reason);
    record.prompt_tokens = answer.usage.as_ref().map(|usage| usage.prompt_tokens);
    record.completion_tokens = answer.usage.as_ref().map(|usage| usage.completion_tokens);
    reject_truncation(&answer, &answered_by, spec, config, prepared.model)?;
    let raw = strip_reasoning_and_log(&answer.content, spec.output.strip_reasoning, logger);
    let output = crate::output::finalize(&spec.output, &raw, &spec.file)?;
    Ok((raw, output, answered_by))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn test_messages(
    spec: &crate::command::CommandSpec,
    config: &crate::config::Config,
    model_id: &str,
    args: &std::collections::BTreeMap<String, String>,
    input: &crate::input::Input,
    env: &dyn Fn(&str) -> Option<String>,
    logger: crate::log::Logger,
) -> crate::Result<Vec<crate::backend::Message>> {
    Ok(prepare_command(
        spec,
        config,
        model_id,
        args,
        env,
        || Ok(input.clone()),
        logger,
        false,
    )?
    .messages)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_test_case(
    spec: &crate::command::CommandSpec,
    config: &crate::config::Config,
    model_id: &str,
    args: &std::collections::BTreeMap<String, String>,
    input: &crate::input::Input,
    env: &dyn Fn(&str) -> Option<String>,
    logger: crate::log::Logger,
    record: &mut crate::stats::Record,
) -> crate::Result<String> {
    let prepared = prepare_command(
        spec,
        config,
        model_id,
        args,
        env,
        || Ok(input.clone()),
        logger,
        false,
    )?;
    run_prepared(spec, config, &prepared, env, logger, None, record).map(|(_, output, _)| output)
}

/// Runs a configured command for a non-CLI caller, without writing to stdout.
pub(crate) fn execute_mcp_command(
    spec: &crate::command::CommandSpec,
    config: &crate::config::Config,
    args: &std::collections::BTreeMap<String, String>,
    input: &str,
    logger: crate::log::Logger,
) -> crate::Result<String> {
    let env = |name: &str| std::env::var(name).ok();
    let mut record = crate::stats::Record::new(spec, &spec.model);
    let result = prepare_command(
        spec,
        config,
        &spec.model,
        args,
        &env,
        || Ok(crate::input::Input::Text(input.to_string())),
        logger,
        true,
    )
    .and_then(|prepared| {
        run_prepared(spec, config, &prepared, &env, logger, None, &mut record)
            .map(|(_, output, _)| output)
    });
    record.finish(&result);
    crate::stats::append(&env, &record, logger);
    result
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
    // `--model` is applied BEFORE resolving and BEFORE reading the input: an
    // unknown override must fail exactly like an unknown model in the
    // command file would, with the network and the input never touched.
    let model_id: &str = leaf_matches
        .get_one::<String>("model")
        .map_or(spec.model.as_str(), String::as_str);
    let mut record = crate::stats::Record::new(spec, model_id);
    let result = run_business_command(
        spec,
        config,
        leaf_matches,
        model_id,
        logger,
        env,
        read_input,
        stdout_is_terminal,
        &mut record,
    );
    // A dry run sends no request: there is nothing to record.
    if !leaf_matches.get_flag("dry-run") {
        // The answer is out before the record is: a slow or failing stats
        // file never delays or alters what a pipe reads.
        let _ = std::io::Write::flush(&mut std::io::stdout());
        record.finish(&result);
        crate::stats::append(env, &record, logger);
    }
    result
}

/// [`execute_business_command`] without the statistics record around it.
#[allow(clippy::too_many_arguments)]
fn run_business_command(
    spec: &crate::command::CommandSpec,
    config: &crate::config::Config,
    leaf_matches: &clap::ArgMatches,
    model_id: &str,
    logger: crate::log::Logger,
    env: &dyn Fn(&str) -> Option<String>,
    read_input: &dyn Fn(
        &crate::command::InputMode,
        Option<&std::path::Path>,
    ) -> crate::Result<String>,
    stdout_is_terminal: bool,
    record: &mut crate::stats::Record,
) -> crate::Result<()> {
    let dry_run = leaf_matches.get_flag("dry-run");
    logger.info(&format!(
        "command \"{}\" -> model \"{}\" from {}{}",
        spec.path.join("/"),
        model_id,
        spec.file.display(),
        if model_id == spec.model {
            String::new()
        } else {
            format!(" (--model override of \"{}\")", spec.model)
        }
    ));

    let args = crate::cli::collect_arg_values(spec, leaf_matches);
    // `env` is injected by the caller (`std::env::var(name).ok()` in
    // production), with the same "missing or empty" semantics
    // `prompt::render`/`prompt::preflight` expect (contract rule 5).

    // The `FILE` argument is only declared (cf. `build_clap_node`) for the
    // modes that accept a file: reproducing the same condition here avoids
    // calling `get_one` on an absent id (panic) and keeps the two spots in
    // sync if a later phase changes one without the other.
    let file_arg = match spec.input {
        crate::command::InputMode::File
        | crate::command::InputMode::StdinOrFile
        | crate::command::InputMode::Binary => leaf_matches
            .get_one::<String>("FILE")
            .map(std::path::Path::new),
        crate::command::InputMode::Stdin => None,
    };
    let mut prepared = prepare_command(
        spec,
        config,
        model_id,
        &args,
        env,
        || match spec.input {
            crate::command::InputMode::Binary => crate::input::resolve_bytes(file_arg),
            _ => read_input(&spec.input, file_arg).map(crate::input::Input::Text),
        },
        logger,
        false,
    )?;
    prepared.no_wait = leaf_matches.get_flag("no-wait");

    if dry_run {
        return print_dry_run(
            prepared.model,
            prepared.backend,
            &prepared.messages,
            prepared.upload(),
            prepared.output_schema.as_ref(),
            spec,
            env,
            stdout_is_terminal,
        );
    }

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
    let streaming = would_stream(spec, stdout_is_terminal);
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
    let (raw_output, output, answered_by) = run_prepared(
        spec,
        config,
        &prepared,
        env,
        logger,
        streaming.then_some(&print_token as TokenSink<'_>),
        record,
    )?;
    logger.info(&format!(
        "output contract honoured ({}): {} characters written to stdout",
        spec.output.format.as_str(),
        output.chars().count()
    ));
    let output = match spec.output.extract.as_deref() {
        Some(pointer) => crate::output::extract(pointer, &output, &spec.file)?,
        None => output,
    };

    write_final_output(
        started.get(),
        stdout_is_terminal,
        &raw_output,
        &output,
        &answered_by,
    );
    Ok(())
}

/// Writes the answer's closing bytes once the pipeline is done: closes the
/// streaming frame already on screen, prints a fresh frame on a terminal
/// that never streamed, or writes the plain answer to a pipe. Split out of
/// `execute_business_command` only to keep it under the crate's line-count
/// lint, same convention as `build_messages` above.
fn write_final_output(
    started: bool,
    stdout_is_terminal: bool,
    raw_output: &str,
    output: &str,
    answered_by: &str,
) {
    if started {
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
        anstream::print!("{}", framed_answer(output, answered_by));
    } else {
        println!("{output}");
    }
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

    fn text_output_spec() -> crate::output::OutputSpec {
        crate::output::OutputSpec {
            format: crate::output::Format::Text,
            max_lines: None,
            strip_reasoning: false,
            ..crate::output::OutputSpec::default()
        }
    }

    #[test]
    fn would_stream_is_true_only_on_a_terminal_with_free_text_no_max_lines_and_no_strip_reasoning()
    {
        let mut spec = crate::command::CommandSpec {
            path: vec!["x".to_string()],
            description: String::new(),
            model: "m".to_string(),
            input: crate::command::InputMode::Stdin,
            prompt: "{{ input }}".to_string(),
            args: std::collections::BTreeMap::new(),
            output: text_output_spec(),
            schemas: std::collections::BTreeMap::new(),
            partials: std::collections::BTreeMap::new(),
            system: None,
            examples: Vec::new(),
            generation: None,
            file: std::path::PathBuf::from("x.md"),
        };

        assert!(would_stream(&spec, true));
        assert!(!would_stream(&spec, false), "a pipe must never stream");

        spec.output.max_lines = Some(1);
        assert!(
            !would_stream(&spec, true),
            "max_lines could reject the answer after it is shown"
        );
        spec.output.max_lines = None;

        spec.output.strip_reasoning = true;
        assert!(
            !would_stream(&spec, true),
            "strip_reasoning must disable streaming even on a terminal"
        );
        spec.output.strip_reasoning = false;

        spec.output.format = crate::output::Format::Json;
        assert!(!would_stream(&spec, true), "a JSON contract never streams");
    }

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
        stub_backend_answering(vec![(status_line.to_string(), body)])
    }

    /// [`stub_backend`] answering one connection per `(status, body)`, in
    /// order.
    fn stub_backend_answering(
        answers: Vec<(String, &'static str)>,
    ) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{BufRead, BufReader, Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind of the stub");
        let addr = listener.local_addr().expect("local address of the stub");

        let handle = std::thread::spawn(move || {
            for (status_line, body) in answers {
                let (mut stream, _) = listener.accept().expect("accepting the connection");
                let mut reader =
                    BufReader::new(stream.try_clone().expect("cloning the TCP stream"));
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
            }
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
        let (url, server, requests) = stub_backend_capturing_request(body);
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            if let Ok((_, captured)) = requests.recv() {
                let _ = tx.send(captured);
            }
            server.join().expect("stub thread");
        });
        (url, handle, rx)
    }

    /// [`stub_backend_capturing_body`], handing back the request's header
    /// lines (lowercased) with its body.
    /// A captured request: its header lines, then its body.
    type CapturedRequest = (Vec<String>, Vec<u8>);

    fn stub_backend_capturing_request(
        body: &'static str,
    ) -> (
        String,
        std::thread::JoinHandle<()>,
        std::sync::mpsc::Receiver<CapturedRequest>,
    ) {
        use std::io::{BufRead, BufReader, Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind of the stub");
        let addr = listener.local_addr().expect("local address of the stub");
        let (tx, rx) = std::sync::mpsc::channel();

        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accepting the connection");
            let mut reader = BufReader::new(stream.try_clone().expect("cloning the TCP stream"));
            let mut content_length = 0usize;
            let mut headers = Vec::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("reading a header line");
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                headers.push(line.trim_end().to_ascii_lowercase());
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = value.trim().parse().unwrap_or(0);
                }
            }
            let mut captured = vec![0u8; content_length];
            reader.read_exact(&mut captured).expect("reading the body");
            tx.send((headers, captured))
                .expect("sending the captured request back");

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
                    protocol: crate::config::Protocol::Chat,
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
                no_wait: false,
                upload: None,
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

    /// The statistics record names the model and backend that ANSWERED,
    /// and carries the answer's usage and finish reason.
    #[test]
    fn the_stats_record_names_the_fallback_that_answered_and_its_usage() {
        let (primary_url, primary_server) = stub_backend("500 Internal Server Error", "{}");
        let (fallback_url, fallback_server) = stub_backend(
            "200 OK",
            r#"{"choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":7,"completion_tokens":2}}"#,
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
        let spec = crate::command::CommandSpec {
            path: vec!["x".to_string()],
            description: String::new(),
            model: "small".to_string(),
            input: crate::command::InputMode::Stdin,
            prompt: "{{ input }}".to_string(),
            args: std::collections::BTreeMap::new(),
            output: crate::output::OutputSpec::default(),
            schemas: std::collections::BTreeMap::new(),
            partials: std::collections::BTreeMap::new(),
            system: None,
            examples: Vec::new(),
            generation: None,
            file: std::path::PathBuf::from("x.md"),
        };
        let mut record = crate::stats::Record::new(&spec, "small");

        execute_test_case(
            &spec,
            &config,
            "small",
            &std::collections::BTreeMap::new(),
            &crate::input::Input::Text("hello".to_string()),
            &|_: &str| None,
            log::Logger::new(log::Level::Error),
            &mut record,
        )
        .expect("the fallback must answer");

        assert_eq!(record.model_answered.as_deref(), Some("big"));
        assert_eq!(record.fallback_used, Some(true));
        assert_eq!(record.backend.as_deref(), Some("gpu"));
        assert_eq!(record.prompt_tokens, Some(7));
        assert_eq!(record.completion_tokens, Some(2));
        assert_eq!(record.finish_reason.as_deref(), Some("stop"));
        assert!(record.duration_ms.is_some());
        primary_server.join().expect("primary stub thread");
        fallback_server.join().expect("fallback stub thread");
    }

    /// A state directory of its own per test, handed over through `env`.
    fn lock_env(name: &str) -> impl Fn(&str) -> Option<String> {
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
            "target/test-fixtures/{name}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        let dir = dir.to_string_lossy().into_owned();
        move |var: &str| matches!(var, "XDG_STATE_HOME" | "HOME").then(|| dir.clone())
    }

    fn ask_once(no_wait: bool) -> Ask<'static> {
        Ask {
            messages: &[],
            schema: None,
            stream: None,
            command_generation: None,
            no_wait,
            upload: None,
        }
    }

    /// The lock is released before the fallback's attempt: a fallback on
    /// the SAME `max_concurrent = 1` backend must not wait for its own
    /// primary.
    #[test]
    fn a_fallback_on_the_same_serialized_backend_does_not_wait_for_its_primary() {
        let (url, server) = stub_backend_answering(vec![
            ("500 Internal Server Error".to_string(), "{}"),
            (
                "200 OK".to_string(),
                r#"{"choices":[{"message":{"role":"assistant","content":"second"}}]}"#,
            ),
        ]);
        let mut backend = backend_at("npu", url);
        backend.max_concurrent = Some(1);
        let mut config = config::Config::default();
        config.backends.insert("npu".to_string(), backend);
        config
            .models
            .insert("small".to_string(), model_on("small", "npu", Some("big")));
        config
            .models
            .insert("big".to_string(), model_on("big", "npu", None));
        let (model, backend) = config.resolve("small").expect("the fixture must resolve");

        let (answer, answered_by) = chat_with_fallback(
            &config,
            model,
            backend,
            &ask_once(false),
            &lock_env("exec-lock-fallback"),
            log::Logger::new(log::Level::Error),
        )
        .expect("the fallback must answer");

        assert_eq!(answer.content, "second");
        assert_eq!(answered_by, "big");
        server.join().expect("stub thread");
    }

    /// `--no-wait` on a backend another process holds: exit 3 naming the
    /// backend, and no request sent.
    #[test]
    fn no_wait_on_a_busy_serialized_backend_fails_with_a_backend_error() {
        let env = lock_env("exec-lock-busy");
        let mut backend = backend_at("npu", "http://127.0.0.1:9".to_string());
        backend.max_concurrent = Some(1);
        let state = crate::runtime::state::StateEnv::from_vars(&env);
        let held = crate::runtime::state::acquire_request_slot(&state, &backend, true, &|| {})
            .expect("the first holder gets the lock")
            .expect("a serialized backend returns a lock");
        let mut config = config::Config::default();
        config.backends.insert("npu".to_string(), backend);
        config
            .models
            .insert("small".to_string(), model_on("small", "npu", None));
        let (model, backend) = config.resolve("small").expect("the fixture must resolve");

        let err = chat_with_fallback(
            &config,
            model,
            backend,
            &ask_once(true),
            &env,
            log::Logger::new(log::Level::Error),
        )
        .expect_err("a busy backend with --no-wait must fail");

        assert_eq!(err.exit_code(), 3);
        assert!(err.to_string().contains("\"npu\""), "got: {err}");
        drop(held);
    }

    fn embeddings_config(url: String) -> config::Config {
        let mut backend = backend_at("emb", url);
        backend.operations = [(
            "embed".to_string(),
            config::Operation {
                method: "POST".to_string(),
                path: "/v1/embeddings".to_string(),
                protocol: config::Protocol::Embeddings,
            },
        )]
        .into_iter()
        .collect();
        let mut model = model_on("vec", "emb", None);
        model.operation = "embed".to_string();
        let mut config = config::Config::default();
        config.backends.insert("emb".to_string(), backend);
        config.models.insert("vec".to_string(), model);
        config
    }

    fn embeddings_spec() -> crate::command::CommandSpec {
        crate::command::CommandSpec {
            path: vec!["embed".to_string()],
            description: String::new(),
            model: "vec".to_string(),
            input: crate::command::InputMode::Stdin,
            prompt: "{{ input }}".to_string(),
            args: std::collections::BTreeMap::new(),
            output: crate::output::OutputSpec {
                format: crate::output::Format::Json,
                ..crate::output::OutputSpec::default()
            },
            schemas: std::collections::BTreeMap::new(),
            partials: std::collections::BTreeMap::new(),
            system: None,
            examples: Vec::new(),
            generation: None,
            file: std::path::PathBuf::from("embed.md"),
        }
    }

    /// An embeddings operation sends `{model, input}` and answers with the
    /// vector as the JSON document.
    #[test]
    fn an_embeddings_command_sends_the_input_and_answers_with_the_vector() {
        let (url, server, body) =
            stub_backend_capturing_body(r#"{"data":[{"embedding":[0.5,-1.0]}]}"#);
        let config = embeddings_config(url);
        let spec = embeddings_spec();
        let mut record = crate::stats::Record::new(&spec, "vec");

        let output = execute_test_case(
            &spec,
            &config,
            "vec",
            &std::collections::BTreeMap::new(),
            &crate::input::Input::Text("café".to_string()),
            &|_: &str| None,
            log::Logger::new(log::Level::Error),
            &mut record,
        )
        .expect("the vector must come back");

        assert_eq!(output, "[0.5,-1.0]");
        let sent: serde_json::Value =
            serde_json::from_slice(&body.recv().expect("the body")).expect("JSON body");
        assert_eq!(
            sent,
            serde_json::json!({"model": "vec-underlying", "input": "café"})
        );
        server.join().expect("stub thread");
    }

    /// A key the embeddings protocol cannot honour is refused before any
    /// request, naming the command file and the model.
    #[test]
    fn an_embeddings_command_declaring_a_chat_only_key_is_rejected() {
        let config = embeddings_config("http://127.0.0.1:9".to_string());
        let mut with_system = embeddings_spec();
        with_system.system = Some("be brief".to_string());
        let mut as_text = embeddings_spec();
        as_text.output.format = crate::output::Format::Text;
        for spec in [with_system, as_text] {
            let (model, backend) = config.resolve("vec").expect("the fixture must resolve");
            let err = check_protocol(&spec, model, backend).expect_err("must be rejected");
            assert_eq!(err.exit_code(), 2);
            let message = err.to_string();
            assert!(message.contains("embed.md"), "got: {message}");
            assert!(message.contains("\"vec\""), "got: {message}");
        }
    }

    fn transcriptions_config(url: String) -> config::Config {
        let mut config = embeddings_config(url);
        for operation in config
            .backends
            .values_mut()
            .flat_map(|b| b.operations.values_mut())
        {
            operation.protocol = config::Protocol::Transcriptions;
            operation.path = "/v1/audio/transcriptions".to_string();
        }
        config
    }

    fn transcriptions_spec() -> crate::command::CommandSpec {
        let mut spec = embeddings_spec();
        spec.input = crate::command::InputMode::Binary;
        spec.prompt = "Names: Ada".to_string();
        spec.output = crate::output::OutputSpec::default();
        spec
    }

    /// A transcription uploads the bytes untouched in a multipart body,
    /// with the prompt as a hint, and answers with the `text` field.
    #[test]
    fn a_transcription_uploads_the_bytes_and_answers_with_the_text() {
        let (url, server, request) = stub_backend_capturing_request(r#"{"text":" Hello Ada. "}"#);
        let config = transcriptions_config(url);
        let spec = transcriptions_spec();
        let mut record = crate::stats::Record::new(&spec, "vec");
        let audio = vec![0u8, 0xff, 0xfe, b'\r', b'\n', 0x80];

        let output = execute_test_case(
            &spec,
            &config,
            "vec",
            &std::collections::BTreeMap::new(),
            &crate::input::Input::Bytes {
                data: audio.clone(),
                name: "memo.wav".to_string(),
            },
            &|_: &str| None,
            log::Logger::new(log::Level::Error),
            &mut record,
        )
        .expect("the transcript must come back");

        assert_eq!(output, "Hello Ada.");
        let (headers, sent) = request.recv().expect("the request");
        let content_types: Vec<&String> = headers
            .iter()
            .filter(|line| line.starts_with("content-type:"))
            .collect();
        assert_eq!(
            content_types.len(),
            1,
            "exactly one content type: {headers:?}"
        );
        let boundary = content_types[0]
            .strip_prefix("content-type: multipart/form-data; boundary=")
            .expect("a multipart content type");
        let text = String::from_utf8_lossy(&sent);
        assert!(text.starts_with(&format!("--{boundary}\r\n")), "{text}");
        assert!(text.ends_with(&format!("\r\n--{boundary}--\r\n")), "{text}");
        assert!(
            headers
                .iter()
                .any(|line| line.starts_with("content-length:")),
            "{headers:?}"
        );
        assert!(
            sent.windows(audio.len())
                .any(|window| window == audio.as_slice()),
            "the bytes must be sent untouched"
        );
        assert!(
            text.contains("name=\"model\"\r\n\r\nvec-underlying\r\n"),
            "{text}"
        );
        assert!(
            text.contains("name=\"prompt\"\r\n\r\nNames: Ada\r\n"),
            "{text}"
        );
        assert!(text.contains("filename=\"memo.wav\""), "{text}");
        server.join().expect("stub thread");
    }

    /// A binary input goes to a transcriptions operation and nothing else,
    /// and a transcriptions operation takes nothing but a binary input.
    #[test]
    fn a_binary_input_and_a_transcriptions_operation_only_go_together() {
        let mut on_chat = transcriptions_spec();
        on_chat.prompt = "{{ input }}".to_string();
        let chat = {
            let mut config = transcriptions_config("http://127.0.0.1:9".to_string());
            for operation in config
                .backends
                .values_mut()
                .flat_map(|b| b.operations.values_mut())
            {
                operation.protocol = config::Protocol::Chat;
            }
            config
        };
        let mut from_stdin = transcriptions_spec();
        from_stdin.input = crate::command::InputMode::Stdin;
        let transcriptions = transcriptions_config("http://127.0.0.1:9".to_string());
        for (spec, config) in [(on_chat, &chat), (from_stdin, &transcriptions)] {
            let (model, backend) = config.resolve("vec").expect("the fixture must resolve");
            let err = check_protocol(&spec, model, backend).expect_err("must be rejected");
            assert_eq!(err.exit_code(), 2);
            assert!(err.to_string().contains("embed.md"), "got: {err}");
        }
    }

    /// A busy primary under `--no-wait` is a backend failure like any
    /// other: the fallback, on another backend, answers.
    #[test]
    fn no_wait_on_a_busy_primary_falls_back_to_its_fallback() {
        let env = lock_env("exec-lock-busy-fallback");
        let (fallback_url, fallback_server) = stub_backend(
            "200 OK",
            r#"{"choices":[{"message":{"role":"assistant","content":"from the gpu"}}]}"#,
        );
        let mut npu = backend_at("npu", "http://127.0.0.1:9".to_string());
        npu.max_concurrent = Some(1);
        let state = crate::runtime::state::StateEnv::from_vars(&env);
        let held = crate::runtime::state::acquire_request_slot(&state, &npu, true, &|| {})
            .expect("the first holder gets the lock");
        let mut config = config::Config::default();
        config.backends.insert("npu".to_string(), npu);
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

        let (answer, answered_by) = chat_with_fallback(
            &config,
            model,
            backend,
            &ask_once(true),
            &env,
            log::Logger::new(log::Level::Error),
        )
        .expect("the fallback must answer");

        assert_eq!(answer.content, "from the gpu");
        assert_eq!(answered_by, "big");
        drop(held);
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
                no_wait: false,
                upload: None,
            },
            &|_: &str| None,
            log::Logger::new(log::Level::Error),
        )
        .expect_err("both backends failing must fail");

        assert!(matches!(err, Error::Backend(_)));
        let message = err.to_string();
        assert!(message.contains("small"), "got: {message}");
        assert!(message.contains("big"), "got: {message}");
        let Error::Backend(backend_err) = &err else {
            unreachable!("expected a backend error, got {err:?}");
        };
        // The fallback's backend, not its model id, with its own status.
        assert_eq!(backend_err.backend.as_deref(), Some("gpu"));
        assert_eq!(backend_err.status, Some(500));
        primary_server.join().expect("primary stub thread");
        fallback_server.join().expect("fallback stub thread");
    }

    /// A truncation by the fallback names the fallback and ITS limit, not
    /// the primary's.
    #[test]
    fn a_truncation_by_the_fallback_names_the_fallback_and_its_limit() {
        let mut config = config::Config::default();
        let mut small = model_on("small", "npu", Some("big"));
        small.generation.max_tokens = Some(111);
        let mut big = model_on("big", "gpu", None);
        big.generation.max_tokens = Some(222);
        config.models.insert("small".to_string(), small);
        config.models.insert("big".to_string(), big);
        config.backends.insert(
            "gpu".to_string(),
            backend_at("gpu", "http://127.0.0.1:9".to_string()),
        );
        let spec = crate::command::CommandSpec {
            path: vec!["x".to_string()],
            description: String::new(),
            model: "small".to_string(),
            input: crate::command::InputMode::Stdin,
            prompt: "{{ input }}".to_string(),
            args: std::collections::BTreeMap::new(),
            output: crate::output::OutputSpec::default(),
            schemas: std::collections::BTreeMap::new(),
            partials: std::collections::BTreeMap::new(),
            system: None,
            examples: Vec::new(),
            generation: None,
            file: std::path::PathBuf::from("x.md"),
        };
        let answer = crate::backend::ChatAnswer {
            content: "cut".to_string(),
            finish_reason: Some("length".to_string()),
            usage: None,
        };

        let err = reject_truncation(&answer, "big", &spec, &config, &config.models["small"])
            .expect_err("a truncated answer must be rejected");

        assert_eq!(err.exit_code(), 4);
        let message = err.to_string();
        assert!(message.contains("\"big\""), "got: {message}");
        assert!(message.contains("222"), "got: {message}");
        assert!(!message.contains("111"), "got: {message}");
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
                no_wait: false,
                upload: None,
            },
            &|_: &str| None,
            log::Logger::new(log::Level::Error),
        )
        .expect_err("a backend failure without fallback must stay a failure");

        assert_eq!(err.exit_code(), 3);
        let Error::Backend(backend_err) = &err else {
            unreachable!("expected a backend error, got {err:?}");
        };
        assert_eq!(backend_err.status, Some(400));
        assert!(backend_err.url.is_some());
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
            partials: std::collections::BTreeMap::new(),
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
            partials: std::collections::BTreeMap::new(),
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
                no_wait: false,
                upload: None,
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
            partials: std::collections::BTreeMap::new(),
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
            partials: std::collections::BTreeMap::new(),
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
            partials: std::collections::BTreeMap::new(),
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
            partials: std::collections::BTreeMap::new(),
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

    fn partial_fixture(name: &str, files: &[(&str, &[u8])]) -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-fixtures")
            .join(format!("exec-{name}-{n}"));
        std::fs::create_dir_all(&dir).expect("fixture directory");
        for (file, content) in files {
            std::fs::write(dir.join(file), content).expect("fixture file");
        }
        dir
    }

    fn spec_with_partial(
        partial: std::path::PathBuf,
        prompt: &str,
        system: Option<&str>,
        examples: Vec<crate::command::Example>,
    ) -> crate::command::CommandSpec {
        crate::command::CommandSpec {
            path: vec!["x".to_string()],
            description: "desc x".to_string(),
            model: "qwen-fast".to_string(),
            input: crate::command::InputMode::Stdin,
            prompt: prompt.to_string(),
            args: std::collections::BTreeMap::new(),
            output: crate::output::OutputSpec::default(),
            schemas: std::collections::BTreeMap::new(),
            partials: std::collections::BTreeMap::from([("style".to_string(), partial)]),
            system: system.map(str::to_string),
            examples,
            generation: None,
            file: std::path::PathBuf::from("commands/x.md"),
        }
    }

    /// A partial is inserted verbatim wherever it is referenced: body,
    /// `system` and an example turn alike.
    #[test]
    fn a_partial_is_inserted_in_the_body_the_system_and_an_example() {
        let dir = partial_fixture("partial-ok", &[("style.md", b"Be terse.")]);
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
        let specs = vec![spec_with_partial(
            dir.join("style.md"),
            "{{ partials.style }} {{ input }}",
            Some("{{ partials.style }}"),
            vec![crate::command::Example {
                user: "{{ partials.style }}".to_string(),
                assistant: "ok".to_string(),
            }],
        )];
        let cli = crate::cli::build_cli(&specs);
        let matches = cli
            .try_get_matches_from(["npu", "x"])
            .expect("the command line must be accepted");
        let (_, leaf_matches) = crate::cli::selected_path(&matches);
        let read_input =
            |_: &crate::command::InputMode, _: Option<&std::path::Path>| Ok("body".to_string());

        execute_business_command(
            &specs[0],
            &config,
            leaf_matches,
            log::Logger::new(log::Level::Error),
            &|_: &str| None,
            &read_input,
            false,
        )
        .expect("must succeed");

        let captured = rx.recv().expect("the stub must report the captured body");
        let body: serde_json::Value =
            serde_json::from_slice(&captured).expect("the body must be JSON");
        let contents: Vec<&str> = body["messages"]
            .as_array()
            .expect("messages array")
            .iter()
            .map(|message| message["content"].as_str().expect("text content"))
            .collect();
        assert_eq!(contents, ["Be terse.", "Be terse.", "ok", "Be terse. body"]);
        server.join().expect("stub server thread");
    }

    /// A missing, non-UTF-8 or templated partial is a configuration error
    /// raised BEFORE the input is read, naming the command file and the
    /// partial file.
    #[test]
    #[allow(clippy::panic)] // the panic is the assertion: read_input must not run.
    fn a_broken_partial_fails_before_read_input_naming_both_files() {
        let dir = partial_fixture(
            "partial-bad",
            &[
                ("templated.md", b"Hi {{ input }}"),
                ("latin1.md", b"caf\xe9"),
            ],
        );
        let mut config = config::Config::default();
        config.backends.insert(
            "b".to_string(),
            backend_at("b", "http://127.0.0.1:9".to_string()),
        );
        config
            .models
            .insert("qwen-fast".to_string(), model_on("qwen-fast", "b", None));

        for partial in [
            dir.join("missing.md"),
            dir.join("templated.md"),
            dir.join("latin1.md"),
        ] {
            let specs = vec![spec_with_partial(
                partial.clone(),
                "{{ partials.style }}",
                None,
                Vec::new(),
            )];
            let cli = crate::cli::build_cli(&specs);
            let matches = cli
                .try_get_matches_from(["npu", "x"])
                .expect("the command line must be accepted");
            let (_, leaf_matches) = crate::cli::selected_path(&matches);
            let read_input = |_: &crate::command::InputMode, _: Option<&std::path::Path>| {
                panic!("read_input must not be called when a partial is broken")
            };

            let err = execute_business_command(
                &specs[0],
                &config,
                leaf_matches,
                log::Logger::new(log::Level::Error),
                &|_: &str| None,
                &read_input,
                false,
            )
            .expect_err("a broken partial must be rejected");

            assert!(matches!(err, Error::Config(_)));
            let message = err.to_string();
            assert!(message.contains("commands/x.md"), "got: {message}");
            assert!(
                message.contains(&partial.display().to_string()),
                "got: {message}"
            );
        }
    }
}

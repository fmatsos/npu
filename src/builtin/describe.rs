//! `npu describe`: the JSON description of a built-in or a configured
//! command, consumed by calling agents rather than read by a human.

use serde::Serialize;

/// A declared argument as serialized by [`describe`]: same fields as
/// `command::ArgSpec`, never that one directly — `ArgSpec` only derives
/// `Deserialize` (it is never written to output elsewhere in this crate),
/// and this file is not allowed to modify `command.rs` to add
/// `Serialize` there (absolute rule of the task: exclusive owner of
/// `builtin/`).
#[derive(Serialize)]
struct DescribeArg {
    short: Option<char>,
    required: bool,
    description: String,
}

/// The output contract as serialized by [`describe`]: same data as
/// `output::OutputSpec`, but with `format` and `schema` already converted
/// to serializable types (`&str`, `Option<String>`) rather than the
/// internal types (`output::Format`, `Option<PathBuf>`), for the same
/// reason as [`DescribeArg`] above.
#[derive(Serialize)]
struct DescribeOutput<'a> {
    format: &'a str,
    schema: Option<String>,
    max_lines: Option<usize>,
    allow_truncated: bool,
    strip_reasoning: bool,
}

/// Where a command comes from: the file that won, and the scope root it was
/// found in — which is what tells a shadowed command apart from its winner.
#[derive(Serialize)]
struct DescribeSource {
    file: String,
    scope: Option<String>,
}

impl DescribeSource {
    /// The scope root is the parent of the `commands/` directory above
    /// `file`; `None` for a file that is not under one (a test fixture).
    fn of(file: &std::path::Path) -> Self {
        let scope = file
            .ancestors()
            .find(|dir| dir.file_name().is_some_and(|name| name == "commands"))
            .and_then(std::path::Path::parent)
            .map(|root| root.display().to_string());
        DescribeSource {
            file: file.display().to_string(),
            scope,
        }
    }
}

/// A built-in, described from the `clap` tree `cli::builtins` builds — the
/// only place built-ins are declared, so this description cannot drift from
/// it.
#[derive(Serialize)]
struct DescribeBuiltin<'a> {
    name: String,
    kind: &'static str,
    description: String,
    args: std::collections::BTreeMap<&'a str, DescribeArg>,
    subcommands: Vec<&'a str>,
    degraded_mode: bool,
}

/// Describes a built-in as JSON. `degraded_mode` says whether it runs with
/// a configuration that failed to load; the caller knows, this module
/// does not dispatch.
pub fn describe_builtin(
    path: &[&str],
    command: &clap::Command,
    degraded_mode: bool,
) -> crate::Result<String> {
    let args = command
        .get_arguments()
        .filter(|arg| !arg.is_hide_set() && !matches!(arg.get_id().as_str(), "help" | "version"))
        .map(|arg| {
            (
                arg.get_id().as_str(),
                DescribeArg {
                    short: arg.get_short(),
                    required: arg.is_required_set(),
                    description: arg.get_help().map(ToString::to_string).unwrap_or_default(),
                },
            )
        })
        .collect();

    let dto = DescribeBuiltin {
        name: path.join("/"),
        kind: "builtin",
        description: command
            .get_about()
            .map(ToString::to_string)
            .unwrap_or_default(),
        args,
        subcommands: command
            .get_subcommands()
            .map(clap::Command::get_name)
            .collect(),
        degraded_mode,
    };
    serde_json::to_string(&dto)
        .map_err(|err| crate::Error::config(format!("description serialization failed: {err}")))
}

/// The complete JSON description of a command, as serialized by
/// [`describe`].
#[derive(Serialize)]
struct Describe<'a> {
    name: String,
    kind: &'static str,
    description: &'a str,
    model: &'a str,
    /// Backend the model points at; `null` when the model is not configured,
    /// which `describe` reports rather than fails on: it is a description.
    backend: Option<&'a str>,
    fallback: Option<&'a str>,
    source: DescribeSource,
    input: &'a str,
    args: std::collections::BTreeMap<&'a str, DescribeArg>,
    output: DescribeOutput<'a>,
    /// The raw `system` template, or `null` when the command declares none
    /// — never resolved: `describe` documents the file, it does not run it.
    system: Option<&'a str>,
    /// Only the COUNT of `[[examples]]`: their content can carry business
    /// data an agent listing commands should not have to receive.
    examples: usize,
    /// The EFFECTIVE generation table: the model's own `[generation]`
    /// merged with this command's override (B3), so an agent sees what
    /// will actually be sent. `null` when the model does not resolve
    /// (unknown id — `describe` reports rather than fails, see `backend`
    /// above).
    generation: Option<DescribeGeneration>,
}

/// The effective `[generation]` table, as serialized by [`describe`]: same
/// data as `config::Generation`, with `extra` converted to plain JSON.
#[derive(Serialize)]
struct DescribeGeneration {
    temperature: Option<f32>,
    max_tokens: Option<u32>,
    seed: Option<u64>,
    top_p: Option<f32>,
    stop: Option<Vec<String>>,
    extra: Option<serde_json::Map<String, serde_json::Value>>,
}

impl From<&crate::config::Generation> for DescribeGeneration {
    fn from(generation: &crate::config::Generation) -> Self {
        Self {
            temperature: generation.temperature,
            max_tokens: generation.max_tokens,
            seed: generation.seed,
            top_p: generation.top_p,
            stop: generation.stop.clone(),
            extra: generation.extra.as_ref().map(crate::config::extra_to_json),
        }
    }
}

/// Describes a dynamically configured command: produces JSON on stdout,
/// serialized by
/// `serde_json` (never built by hand — a manual `format!` could not
/// correctly escape a description or a prompt containing quotes),
/// including the declared
/// arguments (`args`) and the output contract (`output`, with its
/// format, its schema if any, and its line limit if any).
///
/// Does NO resolution by name: this signature takes
/// an already-resolved `CommandSpec` — it is up to the caller
/// (`lib.rs::run`) to look up this `CommandSpec` (with `cli::find_command`,
/// already written and tested there) before calling
/// this function. "describe on an unknown command" is therefore not a
/// behavior this module can produce or test, for lack of receiving a
/// name to resolve.
pub fn describe(
    spec: &crate::command::CommandSpec,
    config: &crate::config::Config,
) -> crate::Result<String> {
    let model = config.models.get(&spec.model);
    let input = match spec.input {
        crate::command::InputMode::Stdin => "stdin",
        crate::command::InputMode::File => "file",
        crate::command::InputMode::StdinOrFile => "stdin_or_file",
    };
    let format = spec.output.format.as_str();

    let args = spec
        .args
        .iter()
        .map(|(name, arg_spec)| {
            (
                name.as_str(),
                DescribeArg {
                    short: arg_spec.short,
                    required: arg_spec.required,
                    description: arg_spec.description.clone(),
                },
            )
        })
        .collect();

    let dto = Describe {
        name: spec.path.join("/"),
        kind: "command",
        description: &spec.description,
        model: &spec.model,
        backend: model.map(|model| model.backend.as_str()),
        fallback: model.and_then(|model| model.fallback.as_deref()),
        source: DescribeSource::of(&spec.file),
        input,
        args,
        output: DescribeOutput {
            format,
            schema: spec
                .output
                .schema
                .as_ref()
                .map(|path| format!("{}", path.display())),
            max_lines: spec.output.max_lines,
            allow_truncated: spec.output.allow_truncated,
            strip_reasoning: spec.output.strip_reasoning,
        },
        system: spec.system.as_deref(),
        examples: spec.examples.len(),
        generation: model.map(|model| {
            DescribeGeneration::from(&crate::config::Generation::merged(
                &model.generation,
                spec.generation.as_ref(),
            ))
        }),
    };

    serde_json::to_string(&dto)
        .map_err(|err| crate::Error::config(format!("description serialization failed: {err}")))
}

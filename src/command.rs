//! Discovery and parsing of commands defined under `commands/**/*.md`.

use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

/// Input resolution mode for a command.
#[derive(Debug, Default, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum InputMode {
    #[default]
    Stdin,
    File,
    StdinOrFile,
}

/// Type of a configured argument, shared by CLI validation and MCP schemas.
#[derive(Debug, Default, Clone, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ArgType {
    #[default]
    String,
    Enum,
    Integer,
    File,
}

impl ArgType {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Enum => "enum",
            Self::Integer => "integer",
            Self::File => "file",
        }
    }
}

/// A CLI argument declared by a command.
///
/// Shared API contract: see the module doc for how this type is
/// populated. `short` is written in TOML as a string (`short = "l"`);
/// `ArgSpec` carries the already-validated `char`, never the raw string —
/// the conversion (with an explicit check that it never truncates) is
/// done by [`convert_args`], not by this `#[derive(Deserialize)]`, which
/// is never exercised directly on TOML (see [`RawArgSpec`]).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArgSpec {
    pub short: Option<char>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub description: String,
    #[serde(default, rename = "type")]
    pub kind: ArgType,
    #[serde(default)]
    pub values: Option<Vec<String>>,
    #[serde(default)]
    pub min: Option<i64>,
    #[serde(default)]
    pub max: Option<i64>,
}

/// A discovered command: its path (derived from the directory tree), the
/// model to use, the input mode, the prompt (file body), the declared CLI
/// arguments (`[args.*]`) and the output contract (`[output]`,
/// see `crate::output`).
#[derive(Debug)]
pub struct CommandSpec {
    pub path: Vec<String>,
    pub description: String,
    pub model: String,
    pub input: InputMode,
    pub prompt: String,
    pub args: BTreeMap<String, ArgSpec>,
    pub output: crate::output::OutputSpec,
    /// `[schemas]` table: each id the prompt may reference through
    /// `{{ schemas.<id> }}`, mapped to its resolved path (same resolution
    /// as `[output].schema`, see [`resolve_schema_path`]). Read lazily, when
    /// the command runs — never at load time.
    pub schemas: BTreeMap<String, std::path::PathBuf>,
    /// `[partials]` table: each id the prompt may reference through
    /// `{{ partials.<id> }}`, mapped to its resolved path. Read lazily, like
    /// `schemas`, and inserted verbatim.
    pub partials: BTreeMap<String, std::path::PathBuf>,
    /// Optional `system` frontmatter key: a templated system-role message
    /// sent before the examples and the rendered body. Same placeholders as
    /// the body (`{{ args.* }}`, `{{ env.* }}`, `{{ schemas.* }}`), except
    /// `{{ input }}`, rejected at load time (the input is the user's turn,
    /// not the system's).
    pub system: Option<String>,
    /// Optional `[[examples]]` array: fixed few-shot user/assistant pairs,
    /// in file order, emitted after `system` and before the rendered body.
    /// Same placeholder rules as `system`.
    pub examples: Vec<Example>,
    /// Optional `[generation]` section: overrides the model's own
    /// `[generation]`, key by key (see `config::Generation::merged`'s
    /// doc) — the one field-by-field merge in the project, an explicit
    /// exception to "replacement, never merge".
    pub generation: Option<crate::config::Generation>,
    /// Path of the source command file (e.g. `.npu/commands/classify.md`)
    /// this `CommandSpec` was parsed from. Needed by `output::finalize`
    /// to name, at real execution time, the command
    /// file that requests a schema that is not found/readable/invalid —
    /// in addition to the resolved path of the schema itself. `parse`
    /// alone does not know this path (it only receives the scope root,
    /// see its doc): it leaves it as `std::path::PathBuf::new()`, and
    /// it's `read_and_parse`, the only caller that knows the file
    /// actually read, that fills it in afterwards. A `CommandSpec`
    /// obtained via `parse` directly (as most tests in this module do,
    /// since they don't care about this field) therefore carries an
    /// empty `PathBuf` — never reached outside `discover`/`discover_scopes`.
    pub file: std::path::PathBuf,
}

/// One `[[examples]]` entry: a fixed user/assistant turn shown to the model
/// before the command's actual body, both templated like `system` (same
/// placeholder rules, see [`CommandSpec::system`]'s doc).
#[derive(Debug, Clone)]
pub struct Example {
    pub user: String,
    pub assistant: String,
}

/// Raw version of `[[examples]]` as written in TOML: `deny_unknown_fields`
/// like every other frontmatter section, both fields required (an example
/// missing either half is not a usable turn).
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
struct RawExample {
    user: String,
    assistant: String,
}

/// Raw version of `ArgSpec` as written in TOML: `short` is a string
/// there, not a `char`. `toml`/`serde` do know how to deserialize a TOML
/// string directly into a `char` (they reject a multi-character string
/// rather than truncating it), but the resulting message doesn't name the
/// offending argument — only the TOML line/column. We therefore
/// deserialize as `String` here, and [`convert_args`] does the conversion
/// with a message naming the argument and the value, like the rest of
/// this module (see `validate_backend` in `config.rs` for the same
/// idiom).
///
/// `deny_unknown_fields` must be repeated here (and not only on
/// `ArgSpec`, never exercised by `serde` on TOML): otherwise a misspelled
/// key under `[args.*]` (e.g. `requred`) would be read and then silently
/// ignored — exactly the kind of defect this project rejects.
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
struct RawArgSpec {
    #[serde(default)]
    short: Option<String>,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    description: String,
    #[serde(default, rename = "type")]
    kind: ArgType,
    #[serde(default)]
    values: Option<Vec<String>>,
    #[serde(default)]
    min: Option<i64>,
    #[serde(default)]
    max: Option<i64>,
}

/// Raw TOML frontmatter, before default values are resolved.
///
/// `deny_unknown_fields`: without it, a misspelled key
/// at the frontmatter's root level (e.g. `descripton` instead of
/// `description`) would be read and then silently ignored — the same
/// defect this project rejects for `[args.*]`
/// (`RawArgSpec`), now extended to the whole frontmatter.
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub(crate) struct Frontmatter {
    #[serde(default)]
    description: String,
    model: String,
    #[serde(default)]
    input: InputSection,
    #[serde(default)]
    args: BTreeMap<String, RawArgSpec>,
    /// `[output]` section: output format,
    /// JSON schema path, `max_lines`. Optional — its absence produces
    /// `OutputSpec::default()` (`convert_output`, below). `schema` is
    /// deserialized as a raw `String` (not yet a resolved `PathBuf`):
    /// it's `convert_output` that resolves it against the scope root,
    /// never `serde`/`toml`.
    #[serde(default)]
    output: Option<RawOutputSpec>,
    /// `[schemas]` table: `<id> = "<name or path>"`, resolved by
    /// [`resolve_schema_path`].
    #[serde(default)]
    schemas: BTreeMap<String, String>,
    /// `[partials]` table: `<id> = "<name or path>"`, a bare name resolving
    /// to `partials/<name>.md` (see [`convert_partials`]).
    #[serde(default)]
    partials: BTreeMap<String, String>,
    /// Optional `system` key: see [`CommandSpec::system`]'s doc.
    #[serde(default)]
    system: Option<String>,
    /// Optional `[[examples]]` array: see [`CommandSpec::examples`]'s doc.
    #[serde(default)]
    examples: Vec<RawExample>,
    /// Optional `[generation]` section: see [`CommandSpec::generation`]'s
    /// doc. Reuses `config::Generation` directly (same shape, same
    /// `deny_unknown_fields`, same validation via
    /// `config::generation_errors`): a command's override is not a
    /// different kind of thing from a model's own `[generation]`.
    #[serde(default)]
    generation: Option<crate::config::Generation>,
}

/// Raw version of the `[output]` section as written in TOML: `schema` is
/// a string there (a path relative to the scope root, or absolute), not
/// yet the resolved `PathBuf` carried by `crate::output::OutputSpec`.
/// Same idiom as [`RawArgSpec`] for `[args.*]`: the conversion (with its
/// own checks and messages naming the section) is done by
/// [`convert_output`], never directly by `#[derive(Deserialize)]`.
///
/// `deny_unknown_fields` (same architecture rule as the rest of the
/// frontmatter): a misspelled key
/// under `[output]` (e.g. `max_line` instead of `max_lines`) must fail at
/// load time, not silently fall back to "no limit".
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
struct RawOutputSpec {
    #[serde(default)]
    format: crate::output::Format,
    #[serde(default)]
    schema: Option<String>,
    #[serde(default)]
    max_lines: Option<usize>,
    /// Accepts a truncated answer (`finish_reason == "length"`) as-is
    /// instead of failing with `Error::Output`. `false` by default.
    #[serde(default)]
    allow_truncated: bool,
    /// Strips one leading `<think>...</think>` block before the rest of
    /// the output pipeline runs, and disables streaming for this command
    /// (see `exec::execute_business_command`). `false` by default.
    #[serde(default)]
    strip_reasoning: bool,
}

/// `[input]` section of the frontmatter.
///
/// `deny_unknown_fields`: a misspelled key here (e.g.
/// `moed` instead of `mode`) would otherwise silently fall back to the
/// default input mode (`InputMode::Stdin`) — the most dangerous case, a
/// file-input command silently reading stdin instead.
#[derive(Debug, Deserialize, Default)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
struct InputSection {
    #[serde(default)]
    mode: Option<InputMode>,
}

/// Discovers commands across several scope roots and merges them by
/// replacement, keyed by full command path.
///
/// `roots` is ordered from most general to most local (see
/// `scope::roots`). The command key (its path, e.g. `"git/review"`) comes
/// from the FILE PATH, not its content: the winner for each path can
/// therefore be resolved across all scopes BEFORE opening a single file.
/// A command whose full path has already been
/// seen in a more general scope is REPLACED wholesale by the more local
/// scope's version — no field-by-field merge. A file that is broken but
/// shadowed by a more local override is therefore never read or parsed.
/// A missing root is not an error: `collect_command_files` already
/// returns an empty `Vec` for a missing `commands/` directory.
///
/// The result is sorted by full path, so that the clap tree and the help
/// output are deterministic regardless of the filesystem's read order.
///
/// Each winner also carries the scope root (`root`) it comes from —
/// needed by `parse` to resolve a relative `[output].schema` —
/// in addition to its file path: the scope root of
/// a nested command (e.g. `git/review.md`) remains the scope root itself,
/// never an ancestor derived from the file path.
pub fn discover_scopes(roots: &[std::path::PathBuf]) -> crate::Result<Vec<CommandSpec>> {
    let mut winners: std::collections::BTreeMap<
        String,
        (Vec<String>, std::path::PathBuf, std::path::PathBuf),
    > = std::collections::BTreeMap::new();

    for root in roots {
        for (path, file) in collect_command_files(root)? {
            let key = path.join("/");
            winners.insert(key, (path, file, root.clone()));
        }
    }

    let mut specs = Vec::with_capacity(winners.len());
    for (path, file, root) in winners.into_values() {
        specs.push(read_and_parse(path, &file, &root)?);
    }

    Ok(specs)
}

/// Walks `<root>/commands/**/*.md` and returns, for each file found, the
/// command path derived from its location (e.g. `["git", "review"]` for
/// `commands/git/review.md`) along with the file path — without reading
/// or parsing its content. Extracted out of `discover` so that
/// `discover_scopes` can resolve the file-path override before opening a
/// single file.
///
/// Returns an empty `Vec` if the `commands/` directory does not exist.
fn collect_command_files(
    root: &std::path::Path,
) -> crate::Result<Vec<(Vec<String>, std::path::PathBuf)>> {
    let commands_root = root.join("commands");
    if !commands_root.is_dir() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    collect_markdown_files(&commands_root, &mut files)?;

    let mut result = Vec::with_capacity(files.len());
    for file in files {
        let relative = file.strip_prefix(&commands_root).map_err(|_| {
            crate::Error::Config(crate::error::ConfigError::in_file(
                &file,
                None::<String>,
                "command path outside commands/",
            ))
        })?;
        let path = relative
            .with_extension("")
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        result.push((path, file));
    }

    Ok(result)
}

/// Rejects a command path whose FIRST segment collides with a name
/// reserved for the CLI's built-ins (`builtin::RESERVED`: `backend`,
/// `config`, `doctor`, `describe`, `update`, `model`, as well as `help`,
/// reserved by `clap` itself).
///
/// Without this rejection, `commands/doctor.md` would be silently
/// shadowed by the `doctor` built-in built in `cli::builtins` (or, depending on
/// the clap tree's build order, would shadow it instead) — a naming
/// conflict that would only surface at execution time, confusingly,
/// rather than being caught at load time like any other configuration
/// error in this module.
///
/// Applies ONLY to the first segment: `commands/git/describe.md` gives
/// the path `["git", "describe"]`, which conflicts with nothing (built-ins
/// only exist at the top level) and stays valid.
///
/// Called first thing from `read_and_parse`, therefore BEFORE any disk
/// read and only on the WINNING files of scope resolution
/// (`discover`/`discover_scopes` only call `read_and_parse` on entries
/// surviving the path merge): a `commands/doctor.md` from a general
/// scope, shadowed by a valid local override at the same path, is never
/// opened by this function — but the local override, itself having
/// "doctor" as its first segment, remains just as rejected. There is no
/// way to make a path with a reserved first segment valid, whatever scope
/// it comes from: that is precisely the point of this rejection (the
/// same "never opened" guarantee as for a broken
/// frontmatter that's shadowed, but NOT the same conclusion — a reserved
/// name stays rejected even as a winner).
fn reject_reserved_path(path: &[String], file: &std::path::Path) -> crate::Result<()> {
    let Some(first) = path.first() else {
        return Ok(());
    };

    if crate::builtin::RESERVED.contains(&first.as_str()) {
        return Err(crate::Error::Config(crate::error::ConfigError::in_file(
            file,
            None::<String>,
            format!(
                "\"{first}\" is reserved for the CLI's built-in commands ({}); rename the \
                 file or move it under a subdirectory (only the first segment of the command \
                 path is reserved, e.g. \"git/{first}.md\" would remain valid)",
                crate::builtin::RESERVED.join(", ")
            ),
        )));
    }

    Ok(())
}

/// Reads and parses the command file `file`, whose derived command path
/// is `path` and whose scope root is `scope_root` (threaded through to
/// `parse`, see its doc, to resolve a relative `[output].schema`).
///
/// Starts with [`reject_reserved_path`], even before reading from disk:
/// a path with a reserved
/// first segment is rejected based on the path alone, without ever
/// needing to open the file.
///
/// Fills in the offending file's path on a `parse` error that does not
/// already carry one: `parse` and the modules it calls into (`prompt::*`,
/// `output::*`) validate a template or a schema, not a file, so their own
/// errors carry `file: None` — this is the one place that knows which file
/// it was reading, and `InFile::in_file` attaches it without re-formatting
/// the message (which would risk a duplicated path if `parse` already named
/// one itself).
fn read_and_parse(
    path: Vec<String>,
    file: &std::path::Path,
    scope_root: &std::path::Path,
) -> crate::Result<CommandSpec> {
    use crate::error::InFile;
    reject_reserved_path(&path, file)?;

    let source = std::fs::read_to_string(file).map_err(|err| {
        crate::Error::Config(crate::error::ConfigError::in_file(
            file,
            None::<String>,
            format!("cannot read file: {err}"),
        ))
    })?;
    let mut spec = parse(&source, path, scope_root).in_file(file)?;
    spec.file = file.to_path_buf();
    Ok(spec)
}

/// Walks `<root>/commands/**/*.md`.
///
/// Returns an empty `Vec` if the `commands/` directory does not exist.
/// `root` also serves as the scope root for resolving a relative
/// `[output].schema`: it's the SAME root
/// passed as the argument, never guessed from each file's path.
pub fn discover(root: &std::path::Path) -> crate::Result<Vec<CommandSpec>> {
    let mut specs = Vec::new();
    for (path, file) in collect_command_files(root)? {
        specs.push(read_and_parse(path, &file, root)?);
    }
    Ok(specs)
}

/// Recursively walks `dir` and accumulates the paths of the `.md` files
/// found into `out`.
fn collect_markdown_files(
    dir: &std::path::Path,
    out: &mut Vec<std::path::PathBuf>,
) -> crate::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_markdown_files(&path, out)?;
        } else if path.extension().is_some_and(|ext| ext == "md") {
            out.push(path);
        }
    }
    Ok(())
}

/// Argument names reserved by `clap` or by `cli::mod`: building the command
/// tree with an argument named `help` or `version` would collide with the
/// flags `clap` handles itself; an argument named `FILE` would collide
/// with the positional `FILE` argument that `cli::mod` (`build_clap_node`)
/// already adds for commands whose input mode accepts a file
/// (`InputMode::File`/`StdinOrFile`); `dry-run` and `model` would collide
/// with the `--dry-run` and `--model` flags `cli::mod` (`build_clap_node`)
/// adds to every business command leaf; `error-format` and `config-dir`
/// would collide with the GLOBAL `--error-format`/`--config-dir` arguments
/// `cli::mod` declares on the root command, same reason as `verbose`.
/// Reject here, at load time, rather than letting the error surface (much
/// less clearly, or even panicking `clap::Command::arg` on a duplicate id)
/// from the clap tree's construction downstream, in `cli::mod`.
const RESERVED_ARG_NAMES: [&str; 8] = [
    "help",
    "version",
    "FILE",
    "verbose",
    "dry-run",
    "model",
    "error-format",
    "config-dir",
];

/// Short letter reserved by `clap`: every `Command` gets an automatic
/// `-h`/`--help` flag, whether or not `disable_help_flag` is called —
/// verified in `cli::mod`, which does not call it. `-V`/`--version` is
/// declared by `cli::builtins` (`cli.version(updater::VERSION)`), but only on the
/// ROOT command: `propagate_version` is never called, so subcommands (the
/// only place a declared `[args.*]` argument lives) never get an automatic
/// `-V`. We therefore do not reserve `V` here, so as
/// not to reject a configuration that collides with nothing actually
/// built. If `cli::builtins` ever starts propagating the version flag to
/// subcommands, this list will need to follow.
///
/// `v` is reserved for a different reason: `cli::mod` declares a GLOBAL
/// `--verbose`/`-v` argument on the root command, inherited by every
/// subcommand. A declared `short = "v"` would collide with it at build time,
/// which `clap` reports by panicking — unacceptable for a configuration
/// error, so it is rejected here instead, naming the argument.
const RESERVED_SHORT_LETTERS: [char; 2] = ['h', 'v'];

/// Validates the name of a declared argument (the `[args.<name>]` key):
/// non-empty, made only of the characters a `{{ args.<name> }}`
/// placeholder can carry, not starting with `-` (which would make `clap`
/// mistake it for a flag), and not one of the reserved names above.
///
/// The character check reuses `prompt::is_valid_name_char` (the SAME
/// rule that recognizes `{{ args.<name> }}`) rather than writing a second
/// one: TOML accepts a quoted table key (`[args."café"]`,
/// `[args."foo.bar"]`) with characters a placeholder never accepts.
/// Without this sharing, such a name would pass validation here (neither
/// form has a space nor starts with `-`) and then fail later, at best
/// with a message pointing at the placeholder rather than the offending
/// declaration ("unknown placeholder" instead of "invalid argument
/// name"), at worst never: a declared but unreferenced argument would
/// then silently load with a name no prompt could ever validly
/// reference.
fn validate_arg_name(name: &str) -> crate::Result<()> {
    if name.is_empty() {
        return Err(crate::Error::config(
            "an argument has an empty name (`[args.\"\"]`): arguments must be named",
        ));
    }
    if !name.chars().all(crate::prompt::is_valid_name_char) {
        return Err(crate::Error::config(format!(
            "argument \"{name}\": an argument name may only contain ASCII letters and \
             digits, \"_\" or \"-\" (the same characters a valid placeholder {{{{ \
             args.<name> }}}} accepts on the prompt side)"
        )));
    }
    if name.starts_with('-') {
        return Err(crate::Error::config(format!(
            "argument \"{name}\": an argument name cannot start with \"-\""
        )));
    }
    if RESERVED_ARG_NAMES.contains(&name) {
        return Err(crate::Error::config(format!(
            "argument \"{name}\": name reserved by clap ({}); choose another name",
            RESERVED_ARG_NAMES.join("/")
        )));
    }
    Ok(())
}

/// Converts a raw `short` (TOML string, possibly absent) into a validated
/// `char`, or returns the configuration error naming the argument `name`
/// and the offending value `raw`. Never truncates silently: an empty or
/// multi-character string is a rejection, not a truncation to the first
/// character.
fn convert_short(name: &str, raw: Option<String>) -> crate::Result<Option<char>> {
    let Some(raw) = raw else {
        return Ok(None);
    };

    let mut chars = raw.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return Err(crate::Error::config(format!(
            "argument \"{name}\": \"short\" must be a single character, got \"{raw}\" \
             ({} character(s))",
            raw.chars().count()
        )));
    };

    if RESERVED_SHORT_LETTERS.contains(&c) {
        return Err(crate::Error::config(format!(
            "argument \"{name}\": short letter \"{c}\" is reserved ({}); \
             choose another one",
            RESERVED_SHORT_LETTERS
                .iter()
                .map(|letter| format!("-{letter}"))
                .collect::<Vec<_>>()
                .join("/")
        )));
    }

    // `clap::Arg::short` refuses `-`: `debug_assert!(s != '-', "short option
    // name cannot be `-`")` (clap_builder 4.6.7, `builder/arg.rs`). A
    // `debug_assert!` only runs in debug profile — `cargo run`/`cargo
    // test`/`make test` panic (exit code 101, out of contract); `make
    // qa`/`cargo build --release` (debug assertions disabled by default)
    // would silently let a useless `-` short flag through (ambiguous with
    // the option prefix itself). Rejecting here, at conversion time,
    // closes both: no dependency on the compilation profile for correct
    // behavior.
    if c == '-' {
        return Err(crate::Error::config(format!(
            "argument \"{name}\": the short letter cannot be \"-\" (confused with the option \
             prefix itself); choose another one"
        )));
    }

    Ok(Some(c))
}

/// Converts the raw `[args.*]` from the frontmatter into `ArgSpec`,
/// validating: each argument's name ([`validate_arg_name`]), the
/// conversion of `short` into a `char` ([`convert_short`]), and the
/// absence of a collision between two arguments declaring the same
/// `short` letter within the same command.
///
/// The iteration over `raw` (a `BTreeMap`) is sorted by argument name, so
/// the collision message is deterministic: it always names the argument
/// already seen (alphabetically earlier) first.
fn convert_args(raw: BTreeMap<String, RawArgSpec>) -> crate::Result<BTreeMap<String, ArgSpec>> {
    let mut args = BTreeMap::new();
    let mut shorts_used: BTreeMap<char, String> = BTreeMap::new();

    for (name, raw_spec) in raw {
        validate_arg_name(&name)?;
        match &raw_spec.kind {
            ArgType::Enum => {
                let values = raw_spec.values.as_ref().ok_or_else(|| {
                    crate::Error::config(format!(
                        "argument \"{name}\": enum requires non-empty values"
                    ))
                })?;
                if values.is_empty()
                    || values.iter().collect::<BTreeSet<_>>().len() != values.len()
                    || raw_spec.min.is_some()
                    || raw_spec.max.is_some()
                {
                    return Err(crate::Error::config(format!(
                        "argument \"{name}\": invalid enum values or integer bounds"
                    )));
                }
            }
            ArgType::Integer => {
                if raw_spec.values.is_some()
                    || raw_spec
                        .min
                        .zip(raw_spec.max)
                        .is_some_and(|(min, max)| min > max)
                {
                    return Err(crate::Error::config(format!(
                        "argument \"{name}\": invalid integer bounds or enum values"
                    )));
                }
            }
            ArgType::String | ArgType::File => {
                if raw_spec.values.is_some() || raw_spec.min.is_some() || raw_spec.max.is_some() {
                    return Err(crate::Error::config(format!(
                        "argument \"{name}\": values and bounds do not apply to this type"
                    )));
                }
            }
        }
        let short = convert_short(&name, raw_spec.short)?;

        if let Some(c) = short
            && let Some(existing) = shorts_used.insert(c, name.clone())
        {
            return Err(crate::Error::config(format!(
                "arguments \"{existing}\" and \"{name}\" share the same short letter \"{c}\""
            )));
        }

        args.insert(
            name,
            ArgSpec {
                short,
                required: raw_spec.required,
                description: raw_spec.description,
                kind: raw_spec.kind,
                values: raw_spec.values,
                min: raw_spec.min,
                max: raw_spec.max,
            },
        );
    }

    Ok(args)
}

/// Resolves a JSON schema declared in `[output].schema` or `[schemas]`
/// against `scope_root` (`schemas/` is a sibling directory of `commands/`,
/// both direct children of the scope root). Three forms:
///
/// - a bare NAME (no path separator, no `.json` suffix, e.g.
///   `"classification"`): `<scope_root>/schemas/<name>.json`;
/// - a RELATIVE path (e.g. `"schemas/classification.json"`): joined to
///   `scope_root` as-is, without inserting `schemas/` a second time;
/// - an ABSOLUTE path: used as-is, never recomposed with `scope_root`.
///
/// PURELY SYNTACTIC: never touches the disk, checks neither the
/// existence, readability nor validity of the resulting file — it's a
/// simple, infallible path composition. Checking existence here, i.e. as
/// early as `discover_scopes`/`discover`, before even the
/// clap tree is built in `build_cli` (see `cli::mod`), would make a schema
/// missing for a SINGLE command, even a general command nobody ever
/// invokes, make `npu --help` fail for the whole CLI —
/// strictly WORSE than a schema present but syntactically
/// broken, which stays tolerated: schema compilation
/// (`output::compile_schema`) is LAZY by design (see `output.rs`'s doc),
/// reserved for the command actually
/// invoked, so a content error (broken JSON) never triggers before use.
/// Existence and compilation are aligned: BOTH
/// LAZY, until the actual execution of the command requesting the schema
/// (`output::compile_schema`, which then produces an `Error::Config`
/// naming both the resolved path AND the offending command file). Same
/// principle as for a broken backend/model shadowed
/// by a more local scope: a broken element belonging to a command nobody
/// invokes must never disable the whole CLI. Exhaustively checking every
/// schema of every scope, invoked or not, is `npu doctor`'s job,
/// not this function's — DO NOT reinstate an existence check here thinking
/// you're fixing an oversight: that would reintroduce exactly the bug
/// this design eliminates.
fn resolve_schema_path(declared: &str, scope_root: &std::path::Path) -> std::path::PathBuf {
    resolve_declared_path(declared, scope_root, "schemas", "json")
}

/// [`resolve_schema_path`]'s rule for any file a command declares by name:
/// a bare name is `<scope_root>/<dir>/<name>.<extension>`, a relative path
/// is joined to `scope_root`, an absolute one is used as-is. `[partials]`
/// uses it with `partials`/`md`.
fn resolve_declared_path(
    declared: &str,
    scope_root: &std::path::Path,
    dir: &str,
    extension: &str,
) -> std::path::PathBuf {
    let declared_path = std::path::Path::new(declared);
    if declared_path.is_absolute() {
        declared_path.to_path_buf()
    } else if is_bare_name(declared, extension) {
        scope_root.join(dir).join(format!("{declared}.{extension}"))
    } else {
        scope_root.join(declared_path)
    }
}

/// Whether `declared` is a bare NAME rather than a path: no path separator
/// and no `.<extension>` suffix. Purely syntactic, like
/// [`resolve_schema_path`].
fn is_bare_name(declared: &str, extension: &str) -> bool {
    !declared.contains(['/', '\\'])
        && !std::path::Path::new(declared)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case(extension))
}

/// Converts the raw `[output]` section of the frontmatter
/// (`RawOutputSpec`) into a resolved and validated
/// `crate::output::OutputSpec`.
///
/// Absence of `[output]` (`raw = None`) => `OutputSpec::default()`
/// (`format = text`, no schema, no limit): the section is optional.
///
/// Forbidden combinations, rejected
/// HERE, at load time:
/// - `schema` declared with `format = "text"`: a schema means nothing on
///   plain text;
/// - `max_lines` declared with `format = "json"`: `max_lines` only
///   applies to text.
///
/// `format = "json"` WITHOUT `schema` is explicitly ALLOWED: the
/// output is then only checked as being well-formed JSON, without schema
/// validation (see `output::finalize_json`, `schema: None`).
///
/// The schema path resolution (`resolve_schema_path`) is only called in
/// the `Json` branch: by construction, `format = "text"` has already
/// rejected any presence of `schema` right above, so there's never a
/// path to resolve for text.
fn convert_output(
    raw: Option<RawOutputSpec>,
    scope_root: &std::path::Path,
) -> crate::Result<crate::output::OutputSpec> {
    let Some(raw) = raw else {
        return Ok(crate::output::OutputSpec::default());
    };

    match raw.format {
        crate::output::Format::Text => {
            if raw.schema.is_some() {
                return Err(crate::Error::config(
                    "[output]: \"schema\" only makes sense with format = \"json\" (a JSON \
                     Schema cannot validate anything on plain text); remove \"schema\" or set \
                     format = \"json\"",
                ));
            }
            Ok(crate::output::OutputSpec {
                format: crate::output::Format::Text,
                schema: None,
                max_lines: raw.max_lines,
                allow_truncated: raw.allow_truncated,
                strip_reasoning: raw.strip_reasoning,
            })
        }
        crate::output::Format::Json => {
            if raw.max_lines.is_some() {
                return Err(crate::Error::config(
                    "[output]: \"max_lines\" only makes sense with format = \"text\" (the \
                     command declares format = \"json\", counted in structure, not lines); \
                     remove \"max_lines\" or set format = \"text\"",
                ));
            }
            let schema = raw
                .schema
                .as_deref()
                .map(|declared| resolve_schema_path(declared, scope_root));
            Ok(crate::output::OutputSpec {
                format: crate::output::Format::Json,
                schema,
                max_lines: None,
                allow_truncated: raw.allow_truncated,
                strip_reasoning: raw.strip_reasoning,
            })
        }
    }
}

/// Converts the raw `[schemas]` table into resolved paths, rejecting an id
/// that `{{ schemas.<id> }}` could never reference (same character rule
/// as an argument name, shared with `prompt`).
fn convert_schemas(
    raw: BTreeMap<String, String>,
    scope_root: &std::path::Path,
) -> crate::Result<BTreeMap<String, std::path::PathBuf>> {
    raw.into_iter()
        .map(|(id, declared)| {
            if id.is_empty() || !id.chars().all(crate::prompt::is_valid_name_char) {
                return Err(crate::Error::config(format!(
                    "[schemas]: invalid schema id \"{id}\": only ASCII letters, digits, '_' \
                     and '-' are accepted"
                )));
            }
            let path = resolve_schema_path(&declared, scope_root);
            Ok((id, path))
        })
        .collect()
}

/// Converts the raw `[partials]` table into resolved paths, with the same
/// id rule as [`convert_schemas`]. Like a schema, the file is only read
/// when the command runs (`prompt::read_partial`).
fn convert_partials(
    raw: BTreeMap<String, String>,
    scope_root: &std::path::Path,
) -> crate::Result<BTreeMap<String, std::path::PathBuf>> {
    raw.into_iter()
        .map(|(id, declared)| {
            if id.is_empty() || !id.chars().all(crate::prompt::is_valid_name_char) {
                return Err(crate::Error::config(format!(
                    "[partials]: invalid partial id \"{id}\": only ASCII letters, digits, '_' \
                     and '-' are accepted"
                )));
            }
            let path = resolve_declared_path(&declared, scope_root, "partials", "md");
            Ok((id, path))
        })
        .collect()
}

/// Validates a `system`/`[[examples]]` template: the same unknown-argument
/// and unknown-schema checks as the body ([`crate::prompt::validate`]),
/// plus a rejection of `{{ input }}` — which has no meaning outside the
/// body (see [`CommandSpec::system`]'s doc). `label` names the offending
/// section in the error message (`"system"`, `"examples[0].user"`, ...).
fn validate_message_template(
    template: &str,
    label: &str,
    declared_args: &BTreeSet<String>,
    declared_schemas: &BTreeSet<String>,
    declared_partials: &BTreeSet<String>,
) -> crate::Result<()> {
    crate::prompt::validate(template, declared_args, declared_schemas, declared_partials)?;
    for placeholder in crate::prompt::placeholders(template)? {
        if matches!(placeholder, crate::prompt::Placeholder::Input) {
            return Err(crate::Error::config(format!(
                "\"{label}\" references {{{{ input }}}}, which has no meaning there (the \
                 input is rendered as the final user message, not part of {label})"
            )));
        }
    }
    Ok(())
}

/// An argument referenced by `{{ args.NAME }}` in ANY templated section of
/// the command (body, `system`, an example) but declared `required =
/// false` is a contradiction within the file itself — rendering can never
/// succeed without the argument, whatever the command line actually typed.
/// See `parse`'s doc for why this is rejected at load time rather than at
/// render time or by silently promoting the argument.
fn reject_optional_arg_placeholders(
    template: &str,
    args: &BTreeMap<String, ArgSpec>,
) -> crate::Result<()> {
    for placeholder in crate::prompt::placeholders(template)? {
        if let crate::prompt::Placeholder::Arg(name) = placeholder {
            // `args` is guaranteed to contain `name`: `prompt::validate`
            // has already run on this same template and would have failed
            // otherwise.
            let spec = &args[&name];
            if !spec.required {
                return Err(crate::Error::config(format!(
                    "argument \"{name}\" referenced by {{{{ args.{name} }}}} but declared \
                     required = false: an argument referenced by the prompt must be \
                     required = true (no default value exists yet for \
                     `[args.*]`)"
                )));
            }
        }
    }
    Ok(())
}

/// Line delimiting the TOML frontmatter of a command file, opening and
/// closing. `---` is what every other Markdown-with-frontmatter tool uses,
/// so an editor highlights the header instead of showing three plus signs
/// as body text.
const FRONTMATTER_DELIMITER: &str = "---";

/// The delimiter used before [`FRONTMATTER_DELIMITER`]. Recognized ONLY to
/// produce a message saying what to change: a file written for the previous
/// version must not be diagnosed as having no frontmatter at all.
const LEGACY_FRONTMATTER_DELIMITER: &str = "+++";

/// Parses the content of a command file.
///
/// The frontmatter is delimited by `---` lines; the header is TOML, the
/// body (after the second delimiter) is the prompt. `scope_root` is the
/// scope root (e.g. `./.npu`) this command file comes
/// from: it is used ONLY to resolve a possible relative
/// `[output].schema`, never for anything
/// else here. Resolving the schema path needs
/// the file's scope root, which is only knowable where the files are
/// collected (`collect_command_files`) — therefore threaded through here
/// by the caller (`read_and_parse`) rather than guessed by walking up
/// from the file path, which would resolve wrong for a nested command
/// (`git/review.md` stays under the SAME scope root as
/// `classify.md`).
pub fn parse(
    source: &str,
    path: Vec<String>,
    scope_root: &std::path::Path,
) -> crate::Result<CommandSpec> {
    let mut lines = source.lines();

    match lines.next() {
        Some(FRONTMATTER_DELIMITER) => {}
        // A file opening with the delimiter used before this version gets its
        // own message: "missing frontmatter" would send the author looking
        // for a missing line that is right there, only spelled differently.
        Some(LEGACY_FRONTMATTER_DELIMITER) => {
            return Err(crate::Error::config(format!(
                "frontmatter delimited by '{LEGACY_FRONTMATTER_DELIMITER}': the delimiter is \
                 now '{FRONTMATTER_DELIMITER}' (opening and closing lines both)"
            )));
        }
        _ => {
            return Err(crate::Error::config(format!(
                "missing frontmatter: the file must start with a '{FRONTMATTER_DELIMITER}' line"
            )));
        }
    }

    let mut header_lines = Vec::new();
    let mut closed = false;
    let mut rest_lines: Vec<&str> = Vec::new();
    for line in lines.by_ref() {
        if line == FRONTMATTER_DELIMITER {
            closed = true;
            break;
        }
        header_lines.push(line);
    }
    if !closed {
        return Err(crate::Error::config(format!(
            "unterminated frontmatter: missing closing '{FRONTMATTER_DELIMITER}' line"
        )));
    }
    rest_lines.extend(lines);

    let header = header_lines.join("\n");
    let frontmatter: Frontmatter = toml::from_str(&header)
        .map_err(|err| crate::Error::config(format!("invalid frontmatter: {err}")))?;

    let prompt = rest_lines.join("\n");
    let prompt = prompt.trim_start_matches('\n').to_string();

    let args = convert_args(frontmatter.args)?;

    // Static placeholder validation: a prompt
    // referencing {{ args.unknown }} must fail HERE, at load time — not
    // at execution time. `parse` only runs on the winning files of scope
    // resolution (see `discover_scopes`), so a file shadowed by a local
    // override, even with a broken placeholder, is still never opened nor
    // validated (test
    // `discover_scopes_broken_placeholder_fully_masked_by_local_scope_resolves_successfully`).
    let declared: BTreeSet<String> = args.keys().cloned().collect();
    let schemas = convert_schemas(frontmatter.schemas, scope_root)?;
    let declared_schemas: BTreeSet<String> = schemas.keys().cloned().collect();
    let partials = convert_partials(frontmatter.partials, scope_root)?;
    let declared_partials: BTreeSet<String> = partials.keys().cloned().collect();
    crate::prompt::validate(&prompt, &declared, &declared_schemas, &declared_partials)?;
    let validate_message = |template: &str, label: &str| {
        validate_message_template(
            template,
            label,
            &declared,
            &declared_schemas,
            &declared_partials,
        )
    };

    // `system` and each `[[examples]]` turn are templated exactly like the
    // body, with one restriction: `{{ input }}` has no meaning there (the
    // input IS the user's turn, rendered separately as the last message) —
    // rejected at load time, naming the offending section, rather than
    // silently rendering to an empty string.
    let system = match frontmatter.system {
        None => None,
        Some(raw) => {
            if raw.trim().is_empty() {
                return Err(crate::Error::config(
                    "\"system\" is present but empty (or blank): remove the key or give it \
                     content",
                ));
            }
            validate_message(&raw, "system")?;
            Some(raw)
        }
    };

    let examples = frontmatter
        .examples
        .into_iter()
        .enumerate()
        .map(|(index, raw)| {
            if raw.user.trim().is_empty() || raw.assistant.trim().is_empty() {
                return Err(crate::Error::config(format!(
                    "[[examples]] entry {index}: both \"user\" and \"assistant\" must be \
                     non-empty"
                )));
            }
            let label_user = format!("examples[{index}].user");
            let label_assistant = format!("examples[{index}].assistant");
            validate_message(&raw.user, &label_user)?;
            validate_message(&raw.assistant, &label_assistant)?;
            Ok(Example {
                user: raw.user,
                assistant: raw.assistant,
            })
        })
        .collect::<crate::Result<Vec<_>>>()?;

    // An argument referenced by the prompt via
    // {{ args.NAME }} but declared `required = false` is a contradiction
    // within the command file itself — a prompt that interpolates NAME
    // can never be rendered without NAME, whatever the command line
    // actually typed. Reject HERE, at load time, naming the argument (the
    // file is added by the caller, see `read_and_parse`) rather than
    // letting the failure happen at render time (where it can occur well
    // after the input was consumed) or, worse, silently
    // promoting the argument to `required = true`: that would honor the
    // configuration differently from how it is declared (a key read then
    // ignored, or a value reinterpreted, is a defect).
    //
    // The underlying reason: `npu doctor`/`npu describe` must
    // be able to say that a command file is broken WITHOUT invoking it. A
    // file whose prompt can never be rendered (whatever the call) is
    // broken; rejecting it at load time makes it detectable for free,
    // before any execution.
    //
    // Deliberate constraint of the CURRENT state, not a permanent truth:
    // the day default values (`default = "..."`) exist for `[args.*]`, an
    // optional argument with a default value will be able to be
    // referenced by the prompt again without contradiction, and this
    // rejection will need to be relaxed accordingly.
    reject_optional_arg_placeholders(&prompt, &args)?;
    if let Some(system) = &system {
        reject_optional_arg_placeholders(system, &args)?;
    }
    for example in &examples {
        reject_optional_arg_placeholders(&example.user, &args)?;
        reject_optional_arg_placeholders(&example.assistant, &args)?;
    }

    // `[output]` section: forbidden
    // combinations, resolving the schema path against `scope_root`, and
    // verification (lazy for schema COMPILATION, not for its existence) —
    // see `convert_output`'s doc.
    let output = convert_output(frontmatter.output, scope_root)?;

    // A command's own `[generation]` (validated the same way as a model's:
    // `stop` non-empty, `extra` neither shadowing a typed key nor carrying
    // a datetime or non-finite float) — merged onto the model's at
    // execution time (`config::Generation::merged`), never here: `parse`
    // does not know which model this command resolves to.
    if let Some(generation) = &frontmatter.generation {
        let label = path.join("/");
        crate::config::generation_errors(generation, &label)?;
    }

    Ok(CommandSpec {
        path,
        description: frontmatter.description,
        model: frontmatter.model,
        input: frontmatter.input.mode.unwrap_or_default(),
        prompt,
        args,
        output,
        schemas,
        partials,
        system,
        examples,
        generation: frontmatter.generation,
        // Filled in by `read_and_parse`, the only caller that knows the
        // path of the file actually read (see the field's doc on
        // `CommandSpec`).
        file: std::path::PathBuf::new(),
    })
}

#[cfg(test)]
#[allow(clippy::expect_used)] // tolerated in tests (see Cargo.toml [lints.clippy]).
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Creates a unique fixture directory under `target/`, so as not to
    /// pollute the repo nor collide between tests run in parallel (same
    /// idiom as `config::tests::fixture_dir`).
    fn fixture_dir(name: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-fixtures")
            .join(format!("command-{name}-{n}"));
        std::fs::create_dir_all(&dir).expect("failed to create fixture directory");
        dir
    }

    /// Dummy scope root for `parse` tests that don't declare any
    /// `[output].schema`: its only constraint is not to exist
    /// (`resolve_schema_path` is never reached as long as no schema is
    /// declared), so a fixed path is enough — no need for a fixture per
    /// test.
    fn test_scope_root() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-fixtures")
            .join("command-test-scope-root-placeholder")
    }

    #[test]
    fn collect_markdown_files_recurses_into_subdirectories() {
        let dir = fixture_dir("collect-nested");
        std::fs::create_dir_all(dir.join("git")).expect("failed to create subdirectory");
        std::fs::write(dir.join("classify.md"), "top-level").expect("failed to write fixture");
        std::fs::write(dir.join("git/review.md"), "nested").expect("failed to write fixture");
        std::fs::write(dir.join("notes.txt"), "ignored: not .md").expect("failed to write fixture");

        let mut found = Vec::new();
        collect_markdown_files(&dir, &mut found).expect("collection should succeed");

        assert_eq!(
            found.len(),
            2,
            "only .md files should be collected, at any depth"
        );
        assert!(found.contains(&dir.join("classify.md")));
        assert!(found.contains(&dir.join("git/review.md")));
    }

    #[test]
    fn discover_finds_nested_command_path() {
        let root = fixture_dir("discover-nested");
        let commands_dir = root.join("commands").join("git");
        std::fs::create_dir_all(&commands_dir).expect("failed to create commands/git directory");
        std::fs::write(
            commands_dir.join("review.md"),
            "---\nmodel = \"qwen-fast\"\n---\nprompt\n",
        )
        .expect("failed to write fixture");

        let specs = discover(&root).expect("discovery should succeed");

        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].path, vec!["git".to_string(), "review".to_string()]);
    }

    #[test]
    fn discover_reports_faulty_file_path_on_broken_frontmatter() {
        // A config read but rejected must name the
        // offending file. `parse` alone cannot do it (it doesn't know the
        // path); it's `discover` that must add it.
        let root = fixture_dir("discover-broken-frontmatter");
        let commands_dir = root.join("commands");
        std::fs::create_dir_all(&commands_dir).expect("failed to create commands directory");
        std::fs::write(commands_dir.join("broken.md"), "no frontmatter at all\n")
            .expect("failed to write fixture");

        let err = discover(&root).expect_err("missing frontmatter should fail");

        assert!(matches!(err, crate::Error::Config(_)));
        let message = err.to_string();
        assert!(
            message.contains("broken.md"),
            "the error message must name the offending file, got: {message}"
        );
    }

    #[test]
    fn parses_nominal_command() {
        let source = "---\ndescription = \"Classify\"\nmodel = \"qwen-fast\"\n\n[input]\nmode = \"stdin_or_file\"\n---\nHello {{ input }}\n";
        let spec =
            parse(source, vec!["classify".to_string()], &test_scope_root()).expect("should parse");
        assert_eq!(spec.description, "Classify");
        assert_eq!(spec.model, "qwen-fast");
        assert!(matches!(spec.input, InputMode::StdinOrFile));
        assert_eq!(spec.prompt, "Hello {{ input }}");
        assert_eq!(spec.path, vec!["classify".to_string()]);
    }

    #[test]
    fn typed_arguments_accept_valid_enum_integer_and_file() {
        let source = "---\nmodel = \"test\"\n[args.language]\ntype = \"enum\"\nvalues = [\"fr\", \"en\"]\n[args.count]\ntype = \"integer\"\nmin = 1\nmax = 20\n[args.context]\ntype = \"file\"\n---\nHello\n";
        let spec = parse(source, vec!["typed".into()], &test_scope_root()).expect("valid types");
        assert!(matches!(spec.args["language"].kind, ArgType::Enum));
        assert!(matches!(spec.args["count"].kind, ArgType::Integer));
        assert!(matches!(spec.args["context"].kind, ArgType::File));
    }

    #[test]
    fn typed_argument_rejects_invalid_combinations_naming_argument() {
        for invalid in [
            "type = \"enum\"\nvalues = []",
            "type = \"enum\"\nvalues = [\"fr\", \"fr\"]",
            "type = \"integer\"\nmin = 20\nmax = 1",
            "type = \"file\"\nmin = 1",
        ] {
            let source = format!("---\nmodel = \"test\"\n[args.choice]\n{invalid}\n---\nHello\n");
            let err = parse(&source, vec!["typed".into()], &test_scope_root())
                .expect_err("invalid type declaration");
            assert!(matches!(err, crate::Error::Config(_)));
            assert!(err.to_string().contains("choice"));
        }
    }

    #[test]
    fn legacy_plus_delimiter_is_a_config_error_naming_both_delimiters() {
        let source = "+++\nmodel = \"qwen-fast\"\n+++\nprompt\n";

        let err = parse(source, vec!["x".to_string()], std::path::Path::new("."))
            .expect_err("the previous delimiter must be rejected");

        assert!(matches!(err, crate::Error::Config(_)));
        let msg = err.to_string();
        assert!(msg.contains("+++"), "got: {msg}");
        assert!(msg.contains("---"), "got: {msg}");
    }

    #[test]
    fn missing_closing_delimiter_is_config_error() {
        let source = "---\nmodel = \"qwen-fast\"\nHello\n";
        let err =
            parse(source, vec!["x".to_string()], &test_scope_root()).expect_err("should fail");
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn missing_frontmatter_is_config_error() {
        let source = "Hello {{ input }}\n";
        let err =
            parse(source, vec!["x".to_string()], &test_scope_root()).expect_err("should fail");
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn default_input_mode_is_stdin() {
        let source = "---\nmodel = \"qwen-fast\"\n---\nprompt\n";
        let spec = parse(source, vec!["x".to_string()], &test_scope_root()).expect("should parse");
        assert!(matches!(spec.input, InputMode::Stdin));
    }

    #[test]
    fn missing_model_field_is_config_error() {
        let source = "---\ndescription = \"no model\"\n---\nprompt\n";
        let err =
            parse(source, vec!["x".to_string()], &test_scope_root()).expect_err("should fail");
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn invalid_toml_is_config_error() {
        let source = "---\nmodel = \n---\nprompt\n";
        let err =
            parse(source, vec!["x".to_string()], &test_scope_root()).expect_err("should fail");
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn nested_path_is_preserved() {
        let source = "---\nmodel = \"qwen-fast\"\n---\nprompt\n";
        let spec = parse(
            source,
            vec!["git".to_string(), "review".to_string()],
            &test_scope_root(),
        )
        .expect("should parse");
        assert_eq!(spec.path, vec!["git".to_string(), "review".to_string()]);
    }

    /// Writes a `<root>/commands/<rel_path>.md` command with the given
    /// minimal frontmatter, creating intermediate directories.
    fn write_command(root: &std::path::Path, rel_path: &str, model: &str, prompt: &str) {
        let file = root.join("commands").join(format!("{rel_path}.md"));
        std::fs::create_dir_all(file.parent().expect("file has a parent"))
            .expect("failed to create intermediate directories");
        std::fs::write(&file, format!("---\nmodel = \"{model}\"\n---\n{prompt}\n"))
            .expect("failed to write fixture");
    }

    /// A `[partials]` table belongs to its command: a bare name resolves
    /// under the scope root the command file was found in, never under a
    /// more local scope that happens to have a file of the same name.
    #[test]
    fn a_partial_resolves_under_its_own_command_scope_not_a_more_local_one() {
        let general = fixture_dir("partials-general");
        let local = fixture_dir("partials-local");
        let command = general.join("commands").join("classify.md");
        std::fs::create_dir_all(command.parent().expect("file has a parent"))
            .expect("failed to create intermediate directories");
        std::fs::write(
            &command,
            "---\nmodel = \"m\"\n[partials]\nstyle = \"style\"\n---\n{{ partials.style }}\n",
        )
        .expect("failed to write fixture");
        std::fs::create_dir_all(local.join("commands")).expect("local commands directory");

        let specs =
            discover_scopes(&[general.clone(), local]).expect("discover_scopes should succeed");

        assert_eq!(
            specs[0].partials.get("style"),
            Some(&general.join("partials").join("style.md"))
        );
    }

    #[test]
    fn discover_scopes_local_redefinition_wins() {
        let general = fixture_dir("scopes-general");
        let local = fixture_dir("scopes-local");
        write_command(&general, "classify", "qwen-general", "general prompt");
        write_command(&local, "classify", "qwen-local", "local prompt");

        let specs = discover_scopes(&[general, local]).expect("discover_scopes should succeed");

        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].model, "qwen-local");
        assert_eq!(specs[0].prompt, "local prompt");
    }

    #[test]
    fn discover_scopes_different_paths_accumulate() {
        let general = fixture_dir("scopes-accumulate-general");
        let local = fixture_dir("scopes-accumulate-local");
        write_command(&general, "classify", "qwen-general", "p1");
        write_command(&local, "summarize", "qwen-local", "p2");

        let specs = discover_scopes(&[general, local]).expect("discover_scopes should succeed");

        let mut paths: Vec<_> = specs.iter().map(|s| s.path.join("/")).collect();
        paths.sort();
        assert_eq!(paths, vec!["classify".to_string(), "summarize".to_string()]);
    }

    #[test]
    fn discover_scopes_nested_command_replaced_by_full_path_no_collision_with_root_sibling() {
        let general = fixture_dir("scopes-nested-general");
        let local = fixture_dir("scopes-nested-local");
        write_command(&general, "git/review", "qwen-general", "general p");
        write_command(&general, "review", "qwen-root", "root p");
        write_command(&local, "git/review", "qwen-local", "local p");

        let specs = discover_scopes(&[general, local]).expect("discover_scopes should succeed");

        assert_eq!(specs.len(), 2);

        let nested = specs
            .iter()
            .find(|s| s.path == vec!["git".to_string(), "review".to_string()])
            .expect("git/review should be present");
        assert_eq!(nested.model, "qwen-local");
        assert_eq!(nested.prompt, "local p");

        let root_level = specs
            .iter()
            .find(|s| s.path == vec!["review".to_string()])
            .expect("root-level review should be present, distinct from git/review");
        assert_eq!(root_level.model, "qwen-root");
        assert_eq!(root_level.prompt, "root p");
    }

    #[test]
    fn discover_scopes_nonexistent_root_is_ignored() {
        let nonexistent = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-fixtures")
            .join("command-scopes-does-not-exist");

        let specs = discover_scopes(&[nonexistent]).expect("discover_scopes should succeed");

        assert!(specs.is_empty());
    }

    #[test]
    fn discover_scopes_result_is_sorted_deterministically() {
        let general = fixture_dir("scopes-sort-general");
        write_command(&general, "zebra", "m", "p");
        write_command(&general, "alpha", "m", "p");
        write_command(&general, "middle/child", "m", "p");

        let specs = discover_scopes(&[general]).expect("discover_scopes should succeed");

        let paths: Vec<_> = specs.iter().map(|s| s.path.join("/")).collect();
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted, "the result must be sorted by full path");
    }

    #[test]
    fn discover_scopes_broken_frontmatter_fully_masked_by_local_scope_resolves_successfully() {
        // A command's key comes from the file path,
        // not its content. A valid local override must therefore shadow a
        // broken general file WITHOUT ever opening it.
        let general = fixture_dir("scopes-broken-frontmatter-masked-general");
        let commands_dir = general.join("commands");
        std::fs::create_dir_all(&commands_dir).expect("failed to create commands directory");
        std::fs::write(commands_dir.join("commit-message.md"), "no frontmatter\n")
            .expect("failed to write fixture");

        let local = fixture_dir("scopes-broken-frontmatter-masked-local");
        write_command(&local, "commit-message", "qwen-local", "local prompt");

        let specs = discover_scopes(&[general, local])
            .expect("the local version should shadow the broken file from the general scope");

        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].model, "qwen-local");
        assert_eq!(specs[0].prompt, "local prompt");
    }

    #[test]
    fn discover_error_message_does_not_double_config_error_prefix() {
        // Guards against wrapping `parse`'s
        // already-formatted error (which already carries "configuration
        // error: ") instead of its message, which would double the prefix.
        let root = fixture_dir("discover-double-prefix");
        let commands_dir = root.join("commands");
        std::fs::create_dir_all(&commands_dir).expect("failed to create commands directory");
        std::fs::write(commands_dir.join("broken.md"), "no frontmatter\n")
            .expect("failed to write fixture");

        let err = discover(&root).expect_err("missing frontmatter should fail");
        let msg = err.to_string();
        assert!(msg.contains("broken.md"));
    }

    // -- reserved names ------------------------------------------------------

    #[test]
    fn discover_rejects_doctor_naming_file_and_reserved_name() {
        let root = fixture_dir("reserved-doctor");
        write_command(&root, "doctor", "qwen-fast", "prompt");

        let err = discover(&root).expect_err("\"doctor\" should be rejected as a reserved name");

        assert!(matches!(err, crate::Error::Config(_)));
        let msg = err.to_string();
        assert!(
            msg.contains("doctor.md"),
            "the message must name the offending file, got: {msg}"
        );
        assert!(
            msg.contains("\"doctor\""),
            "the message must name the conflicting reserved name, got: {msg}"
        );
    }

    #[test]
    fn discover_rejects_the_backend_and_config_groups() {
        for name in ["backend", "config"] {
            let root = fixture_dir(&format!("reserved-{name}"));
            write_command(&root, name, "qwen-fast", "prompt");

            let err = discover(&root).expect_err("a built-in group should be reserved");
            let msg = err.to_string();
            assert!(msg.contains(&format!("{name}.md")), "got: {msg}");
            assert!(msg.contains(&format!("\"{name}\"")), "got: {msg}");
        }
    }

    #[test]
    fn discover_accepts_the_names_the_built_ins_released() {
        // 0.4.0 moved these under `backend`/`config` or behind `--version`:
        // they are ordinary command names again.
        for name in ["serve", "stop", "status", "logs", "models", "version"] {
            let root = fixture_dir(&format!("released-{name}"));
            write_command(&root, name, "qwen-fast", "prompt");

            let specs = discover(&root).expect("a released name must be accepted");
            assert_eq!(specs[0].path, vec![name.to_string()]);
        }
    }

    #[test]
    fn discover_rejects_describe_reserved_name() {
        let root = fixture_dir("reserved-describe");
        write_command(&root, "describe", "qwen-fast", "prompt");

        let err = discover(&root).expect_err("\"describe\" should be rejected as a reserved name");

        let msg = err.to_string();
        assert!(msg.contains("describe.md"), "got: {msg}");
        assert!(msg.contains("\"describe\""), "got: {msg}");
    }

    #[test]
    fn discover_rejects_update_reserved_name() {
        {
            let name = "update";
            let root = fixture_dir(&format!("reserved-{name}"));
            write_command(&root, name, "qwen-fast", "prompt");

            let err = discover(&root).expect_err("the built-in name should be reserved");
            let msg = err.to_string();
            assert!(msg.contains(&format!("{name}.md")), "got: {msg}");
            assert!(msg.contains(&format!("\"{name}\"")), "got: {msg}");
        }
    }

    #[test]
    fn discover_rejects_help_reserved_name() {
        // "help" is not a built-in in the sense of `builtin::doctor`, but
        // is part of `builtin::RESERVED` (reserved by clap itself): the
        // rejection must apply identically.
        let root = fixture_dir("reserved-help");
        write_command(&root, "help", "qwen-fast", "prompt");

        let err = discover(&root).expect_err("\"help\" should be rejected as a reserved name");

        let msg = err.to_string();
        assert!(msg.contains("help.md"), "got: {msg}");
        assert!(msg.contains("\"help\""), "got: {msg}");
    }

    #[test]
    fn nested_reserved_name_segment_is_valid() {
        // The rejection only applies to
        // the FIRST segment. `commands/git/describe.md` gives
        // `npu git describe`, which conflicts with nothing.
        let root = fixture_dir("reserved-nested-valid");
        write_command(&root, "git/describe", "qwen-fast", "prompt");

        let specs = discover(&root).expect("git/describe should not be rejected");

        assert_eq!(specs.len(), 1);
        assert_eq!(
            specs[0].path,
            vec!["git".to_string(), "describe".to_string()]
        );
    }

    #[test]
    fn discover_scopes_reserved_name_rejects_local_winner_without_opening_masked_general_file() {
        // Rejecting a reserved name applies to the WINNER of scope
        // resolution, whatever scope it comes from: there is no way to
        // make a path with a reserved first segment valid by having it
        // "win" from a more local scope (see `reject_reserved_path`'s
        // doc). This test still verifies the usual shadowing guarantee:
        // the general file, shadowed, is NEVER
        // opened — only the winning local file is read, and it's that one
        // (not the general one) that the error message names.
        //
        // The general file deliberately contains broken frontmatter ("no
        // frontmatter"): if it were opened by mistake, the resulting error
        // would name this general file and/or contain a frontmatter
        // parsing hint — neither should appear here.
        let general = fixture_dir("reserved-masked-general");
        let commands_dir = general.join("commands");
        std::fs::create_dir_all(&commands_dir).expect("failed to create commands directory");
        std::fs::write(commands_dir.join("doctor.md"), "no frontmatter\n")
            .expect("failed to write fixture");

        let local = fixture_dir("reserved-masked-local");
        write_command(&local, "doctor", "qwen-local", "local prompt");

        let err = discover_scopes(&[general.clone(), local.clone()])
            .expect_err("\"doctor\" remains reserved even as a local winner");

        let msg = err.to_string();
        assert!(
            msg.contains(local.to_string_lossy().as_ref()),
            "the message must name the winning local file, got: {msg}"
        );
        assert!(
            !msg.contains(general.to_string_lossy().as_ref()),
            "the shadowed general file must never be named, got: {msg}"
        );
    }

    // -- [args.*] --------------------------------------------------------------

    #[test]
    fn parses_full_arg_spec() {
        let source = "---\nmodel = \"qwen-fast\"\n\n[args.language]\nshort = \"l\"\n\
                       required = true\ndescription = \"Target language\"\n---\nHello {{ args.language }}\n";
        let spec =
            parse(source, vec!["translate".to_string()], &test_scope_root()).expect("should parse");

        assert_eq!(spec.args.len(), 1);
        let arg = spec
            .args
            .get("language")
            .expect("the argument should be present");
        assert_eq!(arg.short, Some('l'));
        assert!(arg.required);
        assert_eq!(arg.description, "Target language");
    }

    #[test]
    fn arg_short_absent_is_none() {
        let source = "---\nmodel = \"qwen-fast\"\n\n[args.language]\nrequired = true\n---\nHello {{ args.language }}\n";
        let spec =
            parse(source, vec!["translate".to_string()], &test_scope_root()).expect("should parse");

        assert_eq!(spec.args.get("language").expect("present").short, None);
    }

    #[test]
    fn arg_short_multi_character_is_config_error() {
        let source = "---\nmodel = \"qwen-fast\"\n\n[args.language]\nshort = \"lang\"\n---\nHello {{ args.language }}\n";
        let err = parse(source, vec!["translate".to_string()], &test_scope_root())
            .expect_err("a multi-character short should fail");

        assert!(matches!(err, crate::Error::Config(_)));
        let message = err.to_string();
        assert!(message.contains("language"), "got: {message}");
        assert!(message.contains("lang"), "got: {message}");
    }

    #[test]
    fn arg_short_v_is_config_error_because_verbose_is_global() {
        let source = "---\nmodel = \"qwen-fast\"\n\n[args.value]\nshort = \"v\"\n---\nHello {{ args.value }}\n";

        let err = parse(source, vec!["x".to_string()], std::path::Path::new("."))
            .expect_err("-v is taken by the global --verbose argument");

        assert!(matches!(err, crate::Error::Config(_)));
        assert!(
            err.to_string().contains("value"),
            "the message must name the argument"
        );
    }

    #[test]
    fn arg_named_verbose_is_config_error() {
        let source = "---\nmodel = \"qwen-fast\"\n\n[args.verbose]\nrequired = true\n---\nHello {{ args.verbose }}\n";

        let err = parse(source, vec!["x".to_string()], std::path::Path::new("."))
            .expect_err("\"verbose\" is taken by the global argument");

        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("verbose"));
    }

    #[test]
    fn arg_named_dry_run_is_config_error() {
        let source = "---\nmodel = \"qwen-fast\"\n\n[args.\"dry-run\"]\nrequired = true\n---\nHello {{ args.\"dry-run\" }}\n";

        let err = parse(source, vec!["x".to_string()], std::path::Path::new("."))
            .expect_err("\"dry-run\" collides with the flag every business leaf declares");

        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("dry-run"));
    }

    #[test]
    fn arg_named_model_is_config_error() {
        let source = "---\nmodel = \"qwen-fast\"\n\n[args.model]\nrequired = true\n---\nHello {{ args.model }}\n";

        let err = parse(source, vec!["x".to_string()], std::path::Path::new(".")).expect_err(
            "\"model\" collides with the --model override every business leaf declares",
        );

        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("model"));
    }

    #[test]
    fn arg_named_error_format_is_config_error() {
        let source = "---\nmodel = \"qwen-fast\"\n\n[args.\"error-format\"]\nrequired = true\n---\nHello {{ args.\"error-format\" }}\n";

        let err = parse(source, vec!["x".to_string()], std::path::Path::new("."))
            .expect_err("\"error-format\" collides with the global --error-format argument");

        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("error-format"));
    }

    #[test]
    fn arg_named_config_dir_is_config_error() {
        let source = "---\nmodel = \"qwen-fast\"\n\n[args.\"config-dir\"]\nrequired = true\n---\nHello {{ args.\"config-dir\" }}\n";

        let err = parse(source, vec!["x".to_string()], std::path::Path::new("."))
            .expect_err("\"config-dir\" collides with the global --config-dir argument");

        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("config-dir"));
    }

    #[test]
    fn arg_short_dash_is_config_error_not_a_panic() {
        // `clap::Arg::short('-')` does `debug_assert!(s != '-', ...)`
        // (clap_builder 4.6.7): without this rejection at conversion time,
        // a `short = "-"` would panic (exit code 101, out of contract) in
        // debug profile (`cargo run`/`cargo test`/`make test`), and would
        // silently pass in release profile. Verified empirically on both
        // profiles before this fix.
        let source =
            "---\nmodel = \"qwen-fast\"\n\n[args.x]\nshort = \"-\"\n---\nHello {{ args.x }}\n";
        let err = parse(source, vec!["x".to_string()], &test_scope_root())
            .expect_err("a \"-\" short should fail at load time, never panic");

        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains('x'));
    }

    #[test]
    fn arg_required_defaults_to_false() {
        // The prompt does NOT reference `{{ args.language }}`: an argument
        // referenced by the prompt must be
        // `required = true` (see
        // `referenced_arg_with_required_false_is_a_load_error` below).
        // This test targets only `required`'s default value, so the
        // prompt cannot reference the argument without changing what it
        // tests.
        let source = "---\nmodel = \"qwen-fast\"\n\n[args.language]\nshort = \"l\"\n---\nHello {{ input }}\n";
        let spec =
            parse(source, vec!["translate".to_string()], &test_scope_root()).expect("should parse");

        assert!(!spec.args.get("language").expect("present").required);
    }

    #[test]
    fn arg_name_with_accented_character_is_config_error_even_when_unreferenced() {
        // TOML allows a quoted table key with characters a
        // `{{ args.<name> }}` placeholder never accepts (see
        // `validate_arg_name`'s doc). Such an argument, even if never
        // referenced by the prompt, must fail AT LOAD TIME — otherwise it
        // silently loads with a name no prompt could ever validly
        // reference, exactly the defect the architecture rule
        // targets.
        let source = "---\nmodel = \"qwen-fast\"\n\n[args.\"café\"]\nshort = \"c\"\n---\nHello {{ input }}\n";
        let err = parse(source, vec!["x".to_string()], &test_scope_root())
            .expect_err("an accented argument name should fail at load time");

        assert!(matches!(err, crate::Error::Config(_)));
        let message = err.to_string();
        assert!(message.contains("café"), "got: {message}");
    }

    #[test]
    fn arg_name_with_dot_is_config_error() {
        // Same defect as above with a different character: `.` is valid
        // in a quoted TOML key but delimits a placeholder prefix
        // (`args.`/`env.`) on the `prompt.rs` side.
        let source = "---\nmodel = \"qwen-fast\"\n\n[args.\"foo.bar\"]\n---\nHello {{ input }}\n";
        let err = parse(source, vec!["x".to_string()], &test_scope_root())
            .expect_err("an argument name containing a dot should fail at load time");

        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("foo.bar"));
    }

    #[test]
    fn arg_named_help_is_config_error() {
        let source = "---\nmodel = \"qwen-fast\"\n\n[args.help]\nshort = \"h\"\n---\nHello {{ args.help }}\n";
        let err = parse(source, vec!["x".to_string()], &test_scope_root())
            .expect_err("an argument named \"help\" should fail");

        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("help"));
    }

    #[test]
    fn arg_named_file_is_config_error() {
        // `FILE` is the id of the positional argument `cli::mod` adds for
        // commands accepting a file as input: an argument
        // declared with the same name would collide (duplicate clap id)
        // and must therefore be rejected here, at load time.
        let source = "---\nmodel = \"qwen-fast\"\n\n[args.FILE]\nshort = \"f\"\n---\nHello {{ args.FILE }}\n";
        let err = parse(source, vec!["x".to_string()], &test_scope_root())
            .expect_err("an argument named \"FILE\" should fail");

        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("FILE"));
    }

    #[test]
    fn two_args_sharing_the_same_short_letter_is_config_error() {
        let source = "---\nmodel = \"qwen-fast\"\n\n[args.alpha]\nshort = \"x\"\n\n[args.beta]\n\
                       short = \"x\"\n---\nHello {{ args.alpha }} {{ args.beta }}\n";
        let err = parse(source, vec!["x".to_string()], &test_scope_root())
            .expect_err("two arguments sharing the same short letter should fail");

        assert!(matches!(err, crate::Error::Config(_)));
        let message = err.to_string();
        assert!(message.contains("alpha"), "got: {message}");
        assert!(message.contains("beta"), "got: {message}");
        assert!(message.contains('x'), "got: {message}");
    }

    #[test]
    fn prompt_referencing_undeclared_arg_is_config_error_at_parse_time() {
        let source = "---\nmodel = \"qwen-fast\"\n---\nHello {{ args.language }}\n";
        let err = parse(source, vec!["translate".to_string()], &test_scope_root())
            .expect_err("an undeclared args.* placeholder should fail at parse time");

        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("language"));
    }

    #[test]
    fn declared_but_unreferenced_arg_is_not_an_error() {
        let source =
            "---\nmodel = \"qwen-fast\"\n\n[args.unused]\nshort = \"u\"\n---\nHello {{ input }}\n";
        let spec = parse(source, vec!["x".to_string()], &test_scope_root()).expect("should parse");

        assert_eq!(spec.args.len(), 1);
        assert!(spec.args.contains_key("unused"));
    }

    #[test]
    fn command_without_args_section_has_empty_args() {
        let source = "---\nmodel = \"qwen-fast\"\n---\nprompt\n";
        let spec = parse(source, vec!["x".to_string()], &test_scope_root()).expect("should parse");

        assert!(spec.args.is_empty());
    }

    #[test]
    fn existing_commands_without_args_section_still_parse_non_regression() {
        let source = "---\ndescription = \"Classify\"\nmodel = \"qwen-fast\"\n\n[input]\n\
                       mode = \"stdin_or_file\"\n---\nHello {{ input }}\n";
        let spec =
            parse(source, vec!["classify".to_string()], &test_scope_root()).expect("should parse");

        assert_eq!(spec.description, "Classify");
        assert_eq!(spec.model, "qwen-fast");
        assert!(matches!(spec.input, InputMode::StdinOrFile));
        assert_eq!(spec.prompt, "Hello {{ input }}");
        assert!(spec.args.is_empty());
    }

    #[test]
    fn discover_scopes_broken_placeholder_fully_masked_by_local_scope_resolves_successfully() {
        // Same requirement as for a broken
        // frontmatter, applied to a different failure class: an undeclared
        // {{ args.unknown }} placeholder is
        // detected by `parse` itself. A general scope carrying this
        // defect but fully shadowed by a valid local override must still
        // never be opened nor validated.
        let general = fixture_dir("scopes-broken-placeholder-masked-general");
        let commands_dir = general.join("commands");
        std::fs::create_dir_all(&commands_dir).expect("failed to create commands directory");
        std::fs::write(
            commands_dir.join("translate.md"),
            "---\nmodel = \"qwen-general\"\n---\nHello {{ args.unknown }}\n",
        )
        .expect("failed to write fixture");

        let local = fixture_dir("scopes-broken-placeholder-masked-local");
        write_command(&local, "translate", "qwen-local", "local prompt");

        let specs = discover_scopes(&[general, local])
            .expect("the local version should shadow the broken file from the general scope");

        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].model, "qwen-local");
        assert_eq!(specs[0].prompt, "local prompt");
    }

    #[test]
    fn discover_reports_faulty_file_path_and_declared_args_on_misspelled_placeholder() {
        // Symmetric to `discover_reports_faulty_file_path_on_broken_frontmatter`
        // and the non-shadowed counterpart of
        // `discover_scopes_broken_placeholder_fully_masked_by_local_scope_...`:
        // this is the failure mode targeted by the architecture
        // rule ("a key read then silently ignored is a defect") —
        // a misspelled placeholder ({{ args.langauge }} instead
        // of {{ args.language }}) must fail AT LOAD TIME with a message
        // naming the offending file, the missing argument and the declared
        // arguments (rule 3 of the contract). `parse` alone does not know
        // the file's path; it's `discover` (via `read_and_parse`) that
        // must add it, without doubling the "configuration error: " prefix
        // (same invariant as
        // `discover_error_message_does_not_double_config_error_prefix`).
        let root = fixture_dir("discover-misspelled-placeholder");
        let commands_dir = root.join("commands");
        std::fs::create_dir_all(&commands_dir).expect("failed to create commands directory");
        std::fs::write(
            commands_dir.join("translate.md"),
            "---\nmodel = \"qwen-fast\"\n\n[args.language]\nshort = \"l\"\n---\n\
             Translate into {{ args.langauge }}.\n",
        )
        .expect("failed to write fixture");

        let err = discover(&root).expect_err("a misspelled placeholder should fail");

        assert!(matches!(err, crate::Error::Config(_)));
        let message = err.to_string();
        assert!(
            message.contains("translate.md"),
            "the message must name the offending file, got: {message}"
        );
        assert!(
            message.contains("langauge"),
            "the message must name the referenced unknown argument, got: {message}"
        );
        assert!(
            message.contains("language"),
            "the message must list the declared arguments (including 'language'), got: {message}"
        );
    }

    // -- required = false + referenced by the prompt -------------------------

    #[test]
    fn referenced_arg_with_required_false_is_a_load_error_naming_the_file() {
        // `[args.tone]`
        // with `required = false`, referenced by `{{ args.tone }}`. Must
        // fail AT LOAD TIME, not only at render time — and `discover`
        // must name the offending file (same contract as
        // `discover_reports_faulty_file_path_and_declared_args_on_misspelled_placeholder`).
        let root = fixture_dir("referenced-arg-optional-is-load-error");
        let commands_dir = root.join("commands");
        std::fs::create_dir_all(&commands_dir).expect("failed to create commands directory");
        std::fs::write(
            commands_dir.join("optarg.md"),
            "---\nmodel = \"qwen-fast\"\n\n[args.tone]\nrequired = false\n---\n\
             tone {{ args.tone }}: {{ input }}\n",
        )
        .expect("failed to write fixture");

        let err = discover(&root)
            .expect_err("an argument referenced but required = false should fail at load time");

        assert!(matches!(err, crate::Error::Config(_)));
        let message = err.to_string();
        assert!(
            message.contains("optarg.md"),
            "the message must name the offending file, got: {message}"
        );
        assert!(message.contains("tone"), "got: {message}");
        assert!(
            message.contains("required = true"),
            "the message must explain that a referenced argument must be required = true, \
             got: {message}"
        );
    }

    #[test]
    fn referenced_arg_with_required_true_loads_correctly() {
        let source = "---\nmodel = \"qwen-fast\"\n\n[args.tone]\nrequired = true\n---\ntone {{ args.tone }}: {{ input }}\n";
        let spec =
            parse(source, vec!["optarg".to_string()], &test_scope_root()).expect("should parse");

        assert!(spec.args.get("tone").expect("present").required);
    }

    #[test]
    fn declared_but_unreferenced_arg_with_required_false_stays_valid() {
        // Non-regression: this rejection
        // must only reject arguments REFERENCED by the prompt. A declared
        // but unreferenced argument remains a valid, optional CLI
        // argument (see also `declared_but_unreferenced_arg_is_not_an_error`,
        // which doesn't set `required` explicitly).
        let source = "---\nmodel = \"qwen-fast\"\n\n[args.unused]\nrequired = false\n---\nHello {{ input }}\n";
        let spec = parse(source, vec!["x".to_string()], &test_scope_root()).expect("should parse");

        assert!(!spec.args.get("unused").expect("present").required);
    }

    // -- unknown frontmatter keys ----------------------------------

    #[test]
    fn unknown_root_level_frontmatter_key_is_config_error() {
        let source = "---\nmodel = \"qwen-fast\"\ndescripton = \"typo\"\n---\nprompt\n";
        let err = parse(source, vec!["x".to_string()], &test_scope_root())
            .expect_err("an unknown root key should fail, not be silently ignored");

        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn unknown_input_section_key_is_config_error() {
        let source = "---\nmodel = \"qwen-fast\"\n\n[input]\nmoed = \"stdin\"\n---\nprompt\n";
        let err = parse(source, vec!["x".to_string()], &test_scope_root()).expect_err(
            "an unknown key under [input] should fail, not silently fall back to the \
                          default mode",
        );

        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn output_section_is_accepted_and_now_interpreted() {
        // `[output]` already exists
        // in the versioned fixture `.npu/commands/commit-message.md`
        // (format = "text", max_lines = 1). The three keys must be EFFECTIVE, not just
        // accepted by `deny_unknown_fields`.
        let source = "---\nmodel = \"qwen-fast\"\n\n[output]\nformat = \"text\"\nmax_lines = 1\n\
                       ---\nprompt\n";
        let spec = parse(source, vec!["x".to_string()], &test_scope_root())
            .expect("a valid [output] section should parse");

        assert_eq!(spec.prompt, "prompt");
        assert_eq!(spec.output.format, crate::output::Format::Text);
        assert_eq!(spec.output.max_lines, Some(1));
        assert_eq!(spec.output.schema, None);
    }

    // -- [output] ----------------------------------------------------------------

    #[test]
    fn output_section_absent_yields_default_output_spec() {
        let source = "---\nmodel = \"qwen-fast\"\n---\nprompt\n";
        let spec = parse(source, vec!["x".to_string()], &test_scope_root())
            .expect("the absence of [output] should always parse");

        assert_eq!(spec.output.format, crate::output::Format::Text);
        assert_eq!(spec.output.schema, None);
        assert_eq!(spec.output.max_lines, None);
    }

    #[test]
    fn output_json_with_schema_resolves_relative_to_scope_root_not_cwd_nor_command_file() {
        // The easiest point to get wrong: `schemas/` is a SIBLING directory of `commands/`,
        // both direct children of the SCOPE root — never the cwd, never
        // the command file's own directory. Tested with a NESTED command
        // (`git/review.md`, two levels deep) to prove that the command
        // path's depth doesn't shift the resolution: the schema must
        // resolve to `<scope_root>/schemas/classification.json`, NOT
        // `<scope_root>/commands/git/schemas/classification.json`.
        let root = fixture_dir("output-schema-resolution-nested");
        let schemas_dir = root.join("schemas");
        std::fs::create_dir_all(&schemas_dir).expect("failed to create schemas directory");
        let schema_path = schemas_dir.join("classification.json");
        std::fs::write(&schema_path, r#"{"type": "object"}"#).expect("failed to write schema");

        let source = "---\nmodel = \"qwen-fast\"\n\n[output]\nformat = \"json\"\n\
                       schema = \"schemas/classification.json\"\n---\nprompt\n";
        let spec = parse(source, vec!["git".to_string(), "review".to_string()], &root)
            .expect("an existing schema under schemas/ at the scope root should resolve");

        assert_eq!(spec.output.format, crate::output::Format::Json);
        assert_eq!(
            spec.output.schema.as_deref(),
            Some(schema_path.as_path()),
            "the schema must resolve against the scope root, never the cwd nor the command \
             file's path, whatever the depth of the command path"
        );
    }

    #[test]
    fn output_json_schema_bare_name_resolves_under_scope_schemas_directory() {
        let root = test_scope_root();
        let source = "---\nmodel = \"qwen-fast\"\n\n[output]\nformat = \"json\"\n\
                       schema = \"classification\"\n---\nprompt\n";
        let spec = parse(source, vec!["x".to_string()], &root).expect("a bare name should parse");

        assert_eq!(
            spec.output.schema,
            Some(root.join("schemas").join("classification.json"))
        );
    }

    #[test]
    fn schemas_table_resolves_each_entry_and_is_referenceable_from_the_prompt() {
        let root = test_scope_root();
        let source = "---\nmodel = \"qwen-fast\"\n\n[schemas]\nreport = \"report\"\n\
                       legacy = \"other/legacy.json\"\n---\n{{ schemas.report }} {{ schemas.legacy }}\n";
        let spec =
            parse(source, vec!["x".to_string()], &root).expect("declared schemas should parse");

        assert_eq!(
            spec.schemas.get("report"),
            Some(&root.join("schemas").join("report.json"))
        );
        assert_eq!(
            spec.schemas.get("legacy"),
            Some(&root.join("other").join("legacy.json"))
        );
    }

    #[test]
    fn prompt_referencing_undeclared_schema_is_config_error_naming_it() {
        let source = "---\nmodel = \"qwen-fast\"\n---\n{{ schemas.report }}\n";
        let err = parse(source, vec!["x".to_string()], &test_scope_root())
            .expect_err("an undeclared schema must be rejected at load time");

        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("report"));
    }

    #[test]
    fn schemas_table_invalid_id_is_config_error_naming_it() {
        let source =
            "---\nmodel = \"qwen-fast\"\n\n[schemas]\n\"bad.id\" = \"report\"\n---\nprompt\n";
        let err = parse(source, vec!["x".to_string()], &test_scope_root())
            .expect_err("an id no placeholder can reference must be rejected");

        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("bad.id"));
    }

    #[test]
    fn partials_table_resolves_against_the_command_scope_root_and_is_referenceable() {
        let root = test_scope_root();
        let source = "---\nmodel = \"qwen-fast\"\nsystem = \"{{ partials.style }}\"\n\n\
                      [partials]\nstyle = \"style-guide\"\nglossary = \"shared/glossary.md\"\n\n\
                      [[examples]]\nuser = \"{{ partials.glossary }}\"\nassistant = \"ok\"\n\
                      ---\n{{ partials.style }} {{ input }}\n";
        let spec =
            parse(source, vec!["x".to_string()], &root).expect("declared partials should parse");

        assert_eq!(
            spec.partials.get("style"),
            Some(&root.join("partials").join("style-guide.md"))
        );
        assert_eq!(
            spec.partials.get("glossary"),
            Some(&root.join("shared").join("glossary.md"))
        );
    }

    #[test]
    fn an_undeclared_partial_is_config_error_naming_it_in_body_system_or_example() {
        for source in [
            "---\nmodel = \"qwen-fast\"\n---\n{{ partials.style }}\n",
            "---\nmodel = \"qwen-fast\"\nsystem = \"{{ partials.style }}\"\n---\nbody\n",
            "---\nmodel = \"qwen-fast\"\n[[examples]]\nuser = \"{{ partials.style }}\"\n\
             assistant = \"ok\"\n---\nbody\n",
        ] {
            let err = parse(source, vec!["x".to_string()], &test_scope_root())
                .expect_err("an undeclared partial must be rejected at load time");
            assert!(matches!(err, crate::Error::Config(_)));
            assert!(err.to_string().contains("style"), "got: {err}");
        }
    }

    #[test]
    fn partials_table_invalid_id_is_config_error_naming_it() {
        let source =
            "---\nmodel = \"qwen-fast\"\n\n[partials]\n\"bad.id\" = \"style\"\n---\nprompt\n";
        let err = parse(source, vec!["x".to_string()], &test_scope_root())
            .expect_err("an id no placeholder can reference must be rejected");

        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("bad.id"));
    }

    #[test]
    fn output_json_schema_absolute_path_is_used_as_is_not_joined_with_scope_root() {
        // Uncovered branch of `resolve_schema_path`: an ABSOLUTE path in
        // `[output].schema` must resolve to
        // itself, as-is (see `resolve_schema_path`'s doc).
        // `test_scope_root()` designates a root that doesn't even exist
        // on disk, to prove that resolving an absolute schema never
        // depends on it. Note: `std::path::Path::join` already entirely
        // replaces the base with an absolute argument (documented by the
        // stdlib), so `scope_root.join(declared_path)` alone would
        // produce the same result as with the explicit `is_absolute()`
        // branch — that branch is still spelled out for the readability
        // of the intent, not because it changes behavior. This test
        // therefore locks the observable RESULT (the returned path), not
        // the internal branch taken to get there.
        let root = fixture_dir("output-schema-absolute-path");
        let schema_path = root.join("classification.json");
        std::fs::write(&schema_path, r#"{"type": "object"}"#).expect("failed to write schema");

        let source = format!(
            "---\nmodel = \"qwen-fast\"\n\n[output]\nformat = \"json\"\nschema = {}\n\
             ---\nprompt\n",
            // Quoted by `toml` itself: a Windows backslash or an apostrophe
            // in the checkout path must not break the fixture.
            toml::Value::String(schema_path.display().to_string())
        );
        let spec = parse(&source, vec!["x".to_string()], &test_scope_root())
            .expect("an absolute schema path should resolve without depending on the scope root");

        assert_eq!(
            spec.output.schema.as_deref(),
            Some(schema_path.as_path()),
            "an absolute path must be used as-is, never joined with the scope root"
        );
    }

    #[test]
    fn output_json_without_schema_is_accepted() {
        // `format = "json"` WITHOUT
        // `schema` is explicitly allowed, only checking that the output
        // is well-formed JSON.
        let source = "---\nmodel = \"qwen-fast\"\n\n[output]\nformat = \"json\"\n---\nprompt\n";
        let spec = parse(source, vec!["x".to_string()], &test_scope_root())
            .expect("format = json without schema should parse");

        assert_eq!(spec.output.format, crate::output::Format::Json);
        assert_eq!(spec.output.schema, None);
    }

    #[test]
    fn output_schema_with_text_format_is_config_error() {
        // Forbidden combination: a schema
        // means nothing on plain text.
        let source = "---\nmodel = \"qwen-fast\"\n\n[output]\nformat = \"text\"\n\
                       schema = \"schemas/x.json\"\n---\nprompt\n";
        let err = parse(source, vec!["x".to_string()], &test_scope_root())
            .expect_err("schema with format = text should be rejected at load time");

        assert!(matches!(err, crate::Error::Config(_)));
        let message = err.to_string();
        assert!(message.contains("schema"), "got: {message}");
        assert!(message.contains("text"), "got: {message}");
    }

    #[test]
    fn output_max_lines_with_json_format_is_config_error() {
        // Symmetric forbidden combination: max_lines only applies to text.
        let source = "---\nmodel = \"qwen-fast\"\n\n[output]\nformat = \"json\"\nmax_lines = 3\n\
             ---\nprompt\n";
        let err = parse(source, vec!["x".to_string()], &test_scope_root())
            .expect_err("max_lines with format = json should be rejected at load time");

        assert!(matches!(err, crate::Error::Config(_)));
        let message = err.to_string();
        assert!(message.contains("max_lines"), "got: {message}");
        assert!(message.contains("json"), "got: {message}");
    }

    #[test]
    fn output_schema_pointing_to_missing_file_resolves_lazily_at_load() {
        // `resolve_schema_path` is PURELY
        // SYNTACTIC — a declared but disk-absent schema must not
        // make loading (`parse`) fail, aligned with the schema's lazy
        // compilation (see `resolve_schema_path`'s doc and `output.rs`'s).
        // The resolved path must nonetheless stay correct: it's
        // `output::compile_schema`, at the command's actual execution,
        // that discovers the absence (see the next test,
        // `output_schema_missing_file_error_names_the_command_file_via_discover`).
        let root = fixture_dir("output-schema-missing");
        let source = "---\nmodel = \"qwen-fast\"\n\n[output]\nformat = \"json\"\n\
                       schema = \"schemas/does-not-exist.json\"\n---\nprompt\n";
        let spec = parse(source, vec!["x".to_string()], &root)
            .expect("a missing schema should no longer make loading fail");

        assert_eq!(
            spec.output.schema.as_deref(),
            Some(root.join("schemas").join("does-not-exist.json").as_path()),
            "the resolved path must stay correct even if the file doesn't exist"
        );
    }

    #[test]
    fn output_schema_missing_file_error_names_the_command_file_via_discover() {
        // Locks in the schema resolution contract: loading (`discover`)
        // succeeds even if the declared schema
        // is absent (lazy resolution, see `resolve_schema_path`) —
        // `--help` and every other command in the same scope remain
        // usable. The error occurs at USE, when the command requesting
        // this schema is actually invoked (`output::finalize`, which
        // delegates to `compile_schema`), and must name BOTH paths: the
        // schema's resolved path AND the command file requesting it —
        // same requirement as for a broken frontmatter or an unknown
        // placeholder (the architecture rule applied consistently).
        let root = fixture_dir("output-schema-missing-discover");
        write_command_with_output(
            &root,
            "classify",
            "qwen-fast",
            "prompt",
            "format = \"json\"\nschema = \"schemas/absent.json\"",
        );

        let specs = discover(&root).expect("loading should succeed despite the missing schema");
        let spec = specs
            .iter()
            .find(|s| s.path == vec!["classify".to_string()])
            .expect("the classify command should be present");

        let err = crate::output::finalize(&spec.output, "{}", &spec.file)
            .expect_err("a missing schema should fail AT USE, not at load time");

        assert!(matches!(err, crate::Error::Config(_)));
        let message = err.to_string();
        assert!(
            message.contains("classify.md"),
            "the message must name the offending command file, got: {message}"
        );
        assert!(
            // Joined the way `resolve_schema_path` joins it, so the
            // separators match on Windows too.
            message.contains(&root.join("schemas/absent.json").display().to_string()),
            "the message must also cite the schema's resolved path, got: {message}"
        );
    }

    #[test]
    fn unknown_output_section_key_is_config_error() {
        // Same architecture rule as the rest of the frontmatter
        // (`deny_unknown_fields`), now
        // applied to `[output]`.
        let source = "---\nmodel = \"qwen-fast\"\n\n[output]\nformt = \"json\"\n---\nprompt\n";
        let err = parse(source, vec!["x".to_string()], &test_scope_root())
            .expect_err("an unknown key under [output] should fail, not be ignored");

        assert!(matches!(err, crate::Error::Config(_)));
    }

    /// Writes a `<root>/commands/<rel_path>.md` command with a minimal
    /// frontmatter and a given raw-TOML `[output]` section. Complements
    /// `write_command` (which doesn't declare an `[output]` section) for
    /// tests specifically targeting that section via `discover`.
    fn write_command_with_output(
        root: &std::path::Path,
        rel_path: &str,
        model: &str,
        prompt: &str,
        output_toml: &str,
    ) {
        let file = root.join("commands").join(format!("{rel_path}.md"));
        std::fs::create_dir_all(file.parent().expect("file has a parent"))
            .expect("failed to create intermediate directories");
        std::fs::write(
            &file,
            format!("---\nmodel = \"{model}\"\n\n[output]\n{output_toml}\n---\n{prompt}\n"),
        )
        .expect("failed to write fixture");
    }

    #[test]
    fn real_commit_message_fixture_output_section_is_now_effective() {
        // The versioned
        // fixture `.npu/commands/commit-message.md` declares
        // `[output]` (format = "text", max_lines = 1). It must parse AND
        // yield max_lines = Some(1).
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".npu");
        let commands = discover(&root).expect("the real .npu/ fixture should always load");

        let commit_message = commands
            .iter()
            .find(|spec| spec.path == vec!["commit-message".to_string()])
            .expect("commit-message should be present");

        assert_eq!(commit_message.output.format, crate::output::Format::Text);
        assert_eq!(
            commit_message.output.max_lines,
            Some(1),
            "max_lines must be effective, not just accepted"
        );
        assert_eq!(commit_message.output.schema, None);
    }

    // -- system / [[examples]] -----------------------------------

    #[test]
    fn system_and_examples_parse() {
        let source = r#"---
model = "qwen-fast"
system = "You are a deterministic classifier. Answer with JSON only."

[[examples]]
user = "ticket: printer on fire"
assistant = '{"category":"hardware","confidence":0.98}'

[[examples]]
user = "ticket: mouse missing"
assistant = '{"category":"hardware","confidence":0.5}'
---
Classify: {{ input }}
"#;
        let spec = parse(source, vec!["x".to_string()], &test_scope_root()).expect("must parse");
        assert_eq!(
            spec.system.as_deref(),
            Some("You are a deterministic classifier. Answer with JSON only.")
        );
        assert_eq!(spec.examples.len(), 2);
        assert_eq!(spec.examples[0].user, "ticket: printer on fire");
        assert_eq!(
            spec.examples[1].assistant,
            r#"{"category":"hardware","confidence":0.5}"#
        );
    }

    #[test]
    fn without_system_or_examples_both_are_absent() {
        let source = "---\nmodel = \"qwen-fast\"\n---\n{{ input }}\n";
        let spec = parse(source, vec!["x".to_string()], &test_scope_root()).expect("must parse");
        assert_eq!(spec.system, None);
        assert!(spec.examples.is_empty());
    }

    #[test]
    fn input_placeholder_in_system_is_rejected_at_load_time() {
        let source = "---\nmodel = \"qwen-fast\"\nsystem = \"{{ input }}\"\n---\nbody\n";
        let err = parse(source, vec!["x".to_string()], &test_scope_root()).expect_err("must fail");
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn input_placeholder_in_an_example_is_rejected_at_load_time() {
        let source = r#"---
model = "qwen-fast"

[[examples]]
user = "{{ input }}"
assistant = "ok"
---
body
"#;
        let err = parse(source, vec!["x".to_string()], &test_scope_root()).expect_err("must fail");
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn an_empty_system_is_rejected() {
        let source = "---\nmodel = \"qwen-fast\"\nsystem = \"   \"\n---\nbody\n";
        let err = parse(source, vec!["x".to_string()], &test_scope_root()).expect_err("must fail");
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn an_example_missing_assistant_is_rejected() {
        let source = r#"---
model = "qwen-fast"

[[examples]]
user = "hi"
---
body
"#;
        let err = parse(source, vec!["x".to_string()], &test_scope_root()).expect_err("must fail");
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn an_example_with_an_empty_field_is_rejected() {
        let source = r#"---
model = "qwen-fast"

[[examples]]
user = "hi"
assistant = "   "
---
body
"#;
        let err = parse(source, vec!["x".to_string()], &test_scope_root()).expect_err("must fail");
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn an_undeclared_arg_in_system_is_rejected_at_load_time() {
        let source = "---\nmodel = \"qwen-fast\"\nsystem = \"{{ args.unknown }}\"\n---\nbody\n";
        let err = parse(source, vec!["x".to_string()], &test_scope_root()).expect_err("must fail");
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn an_arg_referenced_only_by_an_example_must_be_required() {
        let source = r#"---
model = "qwen-fast"

[args.lang]
required = false

[[examples]]
user = "{{ args.lang }}"
assistant = "ok"
---
body
"#;
        let err = parse(source, vec!["x".to_string()], &test_scope_root()).expect_err("must fail");
        assert!(matches!(err, crate::Error::Config(_)));
    }
}

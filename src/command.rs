//! Discovery and parsing of commands defined under `commands/**/*.md`.

use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

/// Input resolution mode for a command.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputMode {
    #[default]
    Stdin,
    File,
    StdinOrFile,
}

/// A CLI argument declared by a command (npu-cli-spec.md §11).
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
}

/// A discovered command: its path (derived from the directory tree), the
/// model to use, the input mode, the prompt (file body), the declared CLI
/// arguments (`[args.*]`, phase 3) and the output contract (`[output]`,
/// phase 4 — see `crate::output`).
#[derive(Debug)]
pub struct CommandSpec {
    pub path: Vec<String>,
    pub description: String,
    pub model: String,
    pub input: InputMode,
    pub prompt: String,
    pub args: BTreeMap<String, ArgSpec>,
    pub output: crate::output::OutputSpec,
    /// Path of the source command file (e.g. `.npu/commands/classify.md`)
    /// this `CommandSpec` was parsed from. Needed by `output::finalize`
    /// (L3 review, fix 1) to name, at real execution time, the command
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
/// ignored — exactly the defect the L3 review targets.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawArgSpec {
    #[serde(default)]
    short: Option<String>,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    description: String,
}

/// Raw TOML frontmatter, before default values are resolved.
///
/// `deny_unknown_fields` (L3 review, fix 3): without it, a misspelled key
/// at the frontmatter's root level (e.g. `descripton` instead of
/// `description`) would be read and then silently ignored — exactly the
/// defect this project has rejected since phase 1 for `[args.*]`
/// (`RawArgSpec`), now extended to the whole frontmatter.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Frontmatter {
    #[serde(default)]
    description: String,
    model: String,
    #[serde(default)]
    input: InputSection,
    #[serde(default)]
    args: BTreeMap<String, RawArgSpec>,
    /// `[output]` section (phase 4, npu-cli-spec.md §15): output format,
    /// JSON schema path, `max_lines`. Optional — its absence produces
    /// `OutputSpec::default()` (`convert_output`, below). `schema` is
    /// deserialized as a raw `String` (not yet a resolved `PathBuf`):
    /// it's `convert_output` that resolves it against the scope root
    /// (rule 3 of the shared contract), never `serde`/`toml`.
    #[serde(default)]
    output: Option<RawOutputSpec>,
}

/// Raw version of the `[output]` section as written in TOML: `schema` is
/// a string there (a path relative to the scope root, or absolute), not
/// yet the resolved `PathBuf` carried by `crate::output::OutputSpec`.
/// Same idiom as [`RawArgSpec`] for `[args.*]`: the conversion (with its
/// own checks and messages naming the section) is done by
/// [`convert_output`], never directly by `#[derive(Deserialize)]`.
///
/// `deny_unknown_fields` (same architecture rule as the rest of the
/// frontmatter, from the L3 reviews of phases 1 to 3): a misspelled key
/// under `[output]` (e.g. `max_line` instead of `max_lines`) must fail at
/// load time, not silently fall back to "no limit".
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOutputSpec {
    #[serde(default)]
    format: crate::output::Format,
    #[serde(default)]
    schema: Option<String>,
    #[serde(default)]
    max_lines: Option<usize>,
}

/// `[input]` section of the frontmatter.
///
/// `deny_unknown_fields` (L3 review, fix 3): a misspelled key here (e.g.
/// `moed` instead of `mode`) used to silently fall back to the default
/// input mode (`InputMode::Stdin`) — the most dangerous of the three
/// cases raised in review, a file-input command silently reading stdin
/// instead.
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct InputSection {
    #[serde(default)]
    mode: Option<InputMode>,
}

/// Discovers commands across several scope roots (phase 2,
/// `IMPLEMENTATION.md` decision 3) and merges them by replacement.
///
/// `roots` is ordered from most general to most local (see
/// `scope::roots`). The command key (its path, e.g. `"git/review"`) comes
/// from the FILE PATH, not its content: the winner for each path can
/// therefore be resolved across all scopes BEFORE opening a single file
/// (see L3 review, phase 2). A command whose full path has already been
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
/// needed by `parse` to resolve a relative `[output].schema` (rule 3 of
/// the shared contract) — in addition to its file path: the scope root of
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
/// single file (see L3 review, phase 2).
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
            crate::Error::Config(format!(
                "command path outside commands/: {}",
                file.display()
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
/// reserved for the CLI's built-ins (`builtin::RESERVED`: `doctor`,
/// `models`, `describe`, as well as `help`, reserved by `clap` itself —
/// phase 5, point 2 of the shared contract).
///
/// Without this rejection, `commands/doctor.md` would be silently
/// shadowed by the `doctor` built-in built in `lib.rs` (or, depending on
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
/// it comes from: that is precisely the point of this rejection (see L3
/// review, phase 2, the same "never opened" guarantee as for a broken
/// frontmatter that's shadowed, but NOT the same conclusion — a reserved
/// name stays rejected even as a winner).
fn reject_reserved_path(path: &[String], file: &std::path::Path) -> crate::Result<()> {
    let Some(first) = path.first() else {
        return Ok(());
    };

    if crate::builtin::RESERVED.contains(&first.as_str()) {
        return Err(crate::Error::Config(format!(
            "{}: \"{first}\" is reserved for the CLI's built-in commands ({}); rename the \
             file or move it under a subdirectory (only the first segment of the command \
             path is reserved, e.g. \"git/{first}.md\" would remain valid)",
            file.display(),
            crate::builtin::RESERVED.join(", ")
        )));
    }

    Ok(())
}

/// Reads and parses the command file `file`, whose derived command path
/// is `path` and whose scope root is `scope_root` (threaded through to
/// `parse`, see its doc, to resolve a relative `[output].schema`).
///
/// Starts with [`reject_reserved_path`] (phase 5, point 2 of the shared
/// contract), even before reading from disk: a path with a reserved
/// first segment is rejected based on the path alone, without ever
/// needing to open the file.
///
/// Wraps the MESSAGE of a parsing error with the offending file's path,
/// never the already-formatted error: `parse` returns an `Error::Config`
/// whose `Display` already carries the "configuration error: " prefix;
/// re-wrapping this error (rather than its message) in a new
/// `Error::Config` would duplicate that prefix (see L3 review, phase 2).
fn read_and_parse(
    path: Vec<String>,
    file: &std::path::Path,
    scope_root: &std::path::Path,
) -> crate::Result<CommandSpec> {
    reject_reserved_path(&path, file)?;

    let source = std::fs::read_to_string(file).map_err(|err| {
        crate::Error::Config(format!("cannot read file {}: {err}", file.display()))
    })?;
    let mut spec = parse(&source, path, scope_root).map_err(|err| match err {
        crate::Error::Config(msg) => crate::Error::Config(format!("{}: {msg}", file.display())),
        other => other,
    })?;
    spec.file = file.to_path_buf();
    Ok(spec)
}

/// Walks `<root>/commands/**/*.md`.
///
/// Returns an empty `Vec` if the `commands/` directory does not exist.
/// `root` also serves as the scope root for resolving a relative
/// `[output].schema` (rule 3 of the shared contract): it's the SAME root
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

/// Argument names reserved by `clap` or by `lib.rs`: building the command
/// tree with an argument named `help` or `version` would collide with the
/// flags `clap` handles itself; an argument named `FILE` would collide
/// with the positional `FILE` argument that `lib.rs` (`build_clap_node`)
/// already adds for commands whose input mode accepts a file
/// (`InputMode::File`/`StdinOrFile`, phase 3). Reject here, at load time,
/// rather than letting the error surface (much less clearly, or even
/// panicking `clap::Command::arg` on a duplicate id) from the clap tree's
/// construction downstream, in `lib.rs`.
const RESERVED_ARG_NAMES: [&str; 3] = ["help", "version", "FILE"];

/// Short letter reserved by `clap`: every `Command` gets an automatic
/// `-h`/`--help` flag, whether or not `disable_help_flag` is called —
/// verified in `lib.rs`, which does not call it. `-V`/`--version`, on the
/// other hand, only exists if `Command::version(..)` is called, which
/// `lib.rs` also does not do: we therefore do not reserve `V` here, so as
/// not to reject a configuration that collides with nothing actually
/// built. If `lib.rs` ever starts calling `.version(..)`, this list will
/// need to follow.
const RESERVED_SHORT_LETTERS: [char; 1] = ['h'];

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
        return Err(crate::Error::Config(
            "an argument has an empty name (`[args.\"\"]`): arguments must be named".to_string(),
        ));
    }
    if !name.chars().all(crate::prompt::is_valid_name_char) {
        return Err(crate::Error::Config(format!(
            "argument \"{name}\": an argument name may only contain ASCII letters and \
             digits, \"_\" or \"-\" (the same characters a valid placeholder {{{{ \
             args.<name> }}}} accepts on the prompt side)"
        )));
    }
    if name.starts_with('-') {
        return Err(crate::Error::Config(format!(
            "argument \"{name}\": an argument name cannot start with \"-\""
        )));
    }
    if RESERVED_ARG_NAMES.contains(&name) {
        return Err(crate::Error::Config(format!(
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
        return Err(crate::Error::Config(format!(
            "argument \"{name}\": \"short\" must be a single character, got \"{raw}\" \
             ({} character(s))",
            raw.chars().count()
        )));
    };

    if RESERVED_SHORT_LETTERS.contains(&c) {
        return Err(crate::Error::Config(format!(
            "argument \"{name}\": short letter \"{c}\" is reserved by clap (help, version); \
             choose another one"
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
        return Err(crate::Error::Config(format!(
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
        let short = convert_short(&name, raw_spec.short)?;

        if let Some(c) = short
            && let Some(existing) = shorts_used.insert(c, name.clone())
        {
            return Err(crate::Error::Config(format!(
                "arguments \"{existing}\" and \"{name}\" share the same short letter \"{c}\""
            )));
        }

        args.insert(
            name,
            ArgSpec {
                short,
                required: raw_spec.required,
                description: raw_spec.description,
            },
        );
    }

    Ok(args)
}

/// Resolves the path of a JSON schema declared in `[output].schema`
/// against `scope_root` (rule 3 of the shared contract, §4/§6 of the
/// spec: `schemas/` is a sibling directory of `commands/`, both direct
/// children of the scope root). `declared` already carries the
/// `schemas/` segment (see the §6 example:
/// `schema = "schemas/classification.json"`) — it is therefore joined
/// directly to `scope_root`, without inserting `schemas/` a second time.
/// An ABSOLUTE path in the frontmatter is accepted as-is, never
/// recomposed with `scope_root`.
///
/// PURELY SYNTACTIC: never touches the disk, checks neither the
/// existence, readability nor validity of the resulting file — it's a
/// simple, infallible path composition. This is a DELIBERATE change from
/// this phase's L3 review: the previous version checked existence HERE,
/// i.e. as early as `discover_scopes`/`discover`, that is before even the
/// clap tree is built in `build_cli` (see `lib.rs::run`) — a schema
/// missing for a SINGLE command, even a general command nobody ever
/// invokes, would therefore make `npu --help` fail for the whole CLI.
/// That was strictly WORSE than a schema present but syntactically
/// broken, which stayed tolerated: schema compilation
/// (`output::compile_schema`) is LAZY by design (rule 4 of the shared
/// contract, see `output.rs`'s doc), reserved for the command actually
/// invoked, so a content error (broken JSON) never triggered before use —
/// only existence triggered too early. Both checks are now aligned: BOTH
/// LAZY, until the actual execution of the command requesting the schema
/// (`output::compile_schema`, which then produces an `Error::Config`
/// naming both the resolved path AND the offending command file). Same
/// lesson as the L3 review of phase 2 for a broken backend/model shadowed
/// by a more local scope: a broken element belonging to a command nobody
/// invokes must never disable the whole CLI. Exhaustively checking every
/// schema of every scope, invoked or not, is `npu doctor`'s job (phase 5,
/// out of scope here) — DO NOT reinstate an existence check here thinking
/// you're fixing an oversight: that would reintroduce exactly the bug
/// this fix eliminates.
fn resolve_schema_path(declared: &str, scope_root: &std::path::Path) -> std::path::PathBuf {
    let declared_path = std::path::Path::new(declared);
    if declared_path.is_absolute() {
        declared_path.to_path_buf()
    } else {
        scope_root.join(declared_path)
    }
}

/// Converts the raw `[output]` section of the frontmatter
/// (`RawOutputSpec`) into a resolved and validated
/// `crate::output::OutputSpec`.
///
/// Absence of `[output]` (`raw = None`) => `OutputSpec::default()`
/// (`format = text`, no schema, no limit): the section is optional.
///
/// Forbidden combinations (rule 2 of the shared contract), rejected
/// HERE, at load time:
/// - `schema` declared with `format = "text"`: a schema means nothing on
///   plain text;
/// - `max_lines` declared with `format = "json"`: `max_lines` only
///   applies to text.
///
/// `format = "json"` WITHOUT `schema` is explicitly ALLOWED (rule 2): the
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
                return Err(crate::Error::Config(
                    "[output]: \"schema\" only makes sense with format = \"json\" (a JSON \
                     Schema cannot validate anything on plain text); remove \"schema\" or set \
                     format = \"json\""
                        .to_string(),
                ));
            }
            Ok(crate::output::OutputSpec {
                format: crate::output::Format::Text,
                schema: None,
                max_lines: raw.max_lines,
            })
        }
        crate::output::Format::Json => {
            if raw.max_lines.is_some() {
                return Err(crate::Error::Config(
                    "[output]: \"max_lines\" only makes sense with format = \"text\" (the \
                     command declares format = \"json\", counted in structure, not lines); \
                     remove \"max_lines\" or set format = \"text\""
                        .to_string(),
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
            })
        }
    }
}

/// Parses the content of a command file.
///
/// The frontmatter is delimited by `+++` lines; the header is TOML, the
/// body (after the second delimiter) is the prompt. `scope_root` is the
/// scope root (§4/§6 of the spec, e.g. `./.npu`) this command file comes
/// from: it is used ONLY to resolve a possible relative
/// `[output].schema` (rule 3 of the shared contract), never for anything
/// else here. Public signature change compared to phases 1 to 3 (phase 4
/// plan review, shared API contract): resolving the schema path needs
/// the file's scope root, which is only knowable where the files are
/// collected (`collect_command_files`) — therefore threaded through here
/// by the caller (`read_and_parse`) rather than guessed by walking up
/// from the file path, which would resolve wrong for a nested command
/// (§4/§6, `git/review.md` stays under the SAME scope root as
/// `classify.md`).
pub fn parse(
    source: &str,
    path: Vec<String>,
    scope_root: &std::path::Path,
) -> crate::Result<CommandSpec> {
    let mut lines = source.lines();

    match lines.next() {
        Some("+++") => {}
        _ => {
            return Err(crate::Error::Config(
                "missing frontmatter: the file must start with a '+++' line".to_string(),
            ));
        }
    }

    let mut header_lines = Vec::new();
    let mut closed = false;
    let mut rest_lines: Vec<&str> = Vec::new();
    for line in lines.by_ref() {
        if line == "+++" {
            closed = true;
            break;
        }
        header_lines.push(line);
    }
    if !closed {
        return Err(crate::Error::Config(
            "unterminated frontmatter: missing closing '+++' line".to_string(),
        ));
    }
    rest_lines.extend(lines);

    let header = header_lines.join("\n");
    let frontmatter: Frontmatter = toml::from_str(&header)
        .map_err(|err| crate::Error::Config(format!("invalid frontmatter: {err}")))?;

    let prompt = rest_lines.join("\n");
    let prompt = prompt.trim_start_matches('\n').to_string();

    let args = convert_args(frontmatter.args)?;

    // Static placeholder validation (npu-cli-spec.md §12): a prompt
    // referencing {{ args.unknown }} must fail HERE, at load time — not
    // at execution time. `parse` only runs on the winning files of scope
    // resolution (see `discover_scopes`), so a file shadowed by a local
    // override, even with a broken placeholder, is still never opened nor
    // validated (see L3 review, phase 2; test
    // `discover_scopes_broken_placeholder_fully_masked_by_local_scope_resolves_successfully`).
    let declared: BTreeSet<String> = args.keys().cloned().collect();
    crate::prompt::validate(&prompt, &declared)?;

    // L3 review, fix 2: an argument referenced by the prompt via
    // {{ args.NAME }} but declared `required = false` is a contradiction
    // within the command file itself — a prompt that interpolates NAME
    // can never be rendered without NAME, whatever the command line
    // actually typed. Reject HERE, at load time, naming the argument (the
    // file is added by the caller, see `read_and_parse`) rather than
    // letting the failure happen at render time (where it can occur well
    // after the input was consumed, see fix 1) or, worse, silently
    // promoting the argument to `required = true`: that would honor the
    // configuration differently from how it is declared, the same rule
    // this project has applied since phase 1 (a key read then ignored, or
    // a value reinterpreted, is a defect).
    //
    // The underlying reason: in phase 5, `npu doctor`/`npu describe` must
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
    for placeholder in crate::prompt::placeholders(&prompt)? {
        if let crate::prompt::Placeholder::Arg(name) = placeholder {
            // `declared` is guaranteed to contain `name`: `prompt::validate`
            // above would already have failed otherwise.
            let spec = &args[&name];
            if !spec.required {
                return Err(crate::Error::Config(format!(
                    "argument \"{name}\" referenced by {{{{ args.{name} }}}} but declared \
                     required = false: an argument referenced by the prompt must be \
                     required = true (no default value exists yet for \
                     `[args.*]`)"
                )));
            }
        }
    }

    // `[output]` section (phase 4, npu-cli-spec.md §15): forbidden
    // combinations, resolving the schema path against `scope_root`, and
    // verification (lazy for schema COMPILATION, not for its existence) —
    // see `convert_output`'s doc.
    let output = convert_output(frontmatter.output, scope_root)?;

    Ok(CommandSpec {
        path,
        description: frontmatter.description,
        model: frontmatter.model,
        input: frontmatter.input.mode.unwrap_or_default(),
        prompt,
        args,
        output,
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
            "+++\nmodel = \"qwen-fast\"\n+++\nprompt\n",
        )
        .expect("failed to write fixture");

        let specs = discover(&root).expect("discovery should succeed");

        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].path, vec!["git".to_string(), "review".to_string()]);
    }

    #[test]
    fn discover_reports_faulty_file_path_on_broken_frontmatter() {
        // L3 review (phase 1): a config read but rejected must name the
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
        let source = "+++\ndescription = \"Classify\"\nmodel = \"qwen-fast\"\n\n[input]\nmode = \"stdin_or_file\"\n+++\nHello {{ input }}\n";
        let spec =
            parse(source, vec!["classify".to_string()], &test_scope_root()).expect("should parse");
        assert_eq!(spec.description, "Classify");
        assert_eq!(spec.model, "qwen-fast");
        assert!(matches!(spec.input, InputMode::StdinOrFile));
        assert_eq!(spec.prompt, "Hello {{ input }}");
        assert_eq!(spec.path, vec!["classify".to_string()]);
    }

    #[test]
    fn missing_closing_delimiter_is_config_error() {
        let source = "+++\nmodel = \"qwen-fast\"\nHello\n";
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
        let source = "+++\nmodel = \"qwen-fast\"\n+++\nprompt\n";
        let spec = parse(source, vec!["x".to_string()], &test_scope_root()).expect("should parse");
        assert!(matches!(spec.input, InputMode::Stdin));
    }

    #[test]
    fn missing_model_field_is_config_error() {
        let source = "+++\ndescription = \"no model\"\n+++\nprompt\n";
        let err =
            parse(source, vec!["x".to_string()], &test_scope_root()).expect_err("should fail");
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn invalid_toml_is_config_error() {
        let source = "+++\nmodel = \n+++\nprompt\n";
        let err =
            parse(source, vec!["x".to_string()], &test_scope_root()).expect_err("should fail");
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn nested_path_is_preserved() {
        let source = "+++\nmodel = \"qwen-fast\"\n+++\nprompt\n";
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
        std::fs::write(&file, format!("+++\nmodel = \"{model}\"\n+++\n{prompt}\n"))
            .expect("failed to write fixture");
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
        // L3 review (phase 2): a command's key comes from the file path,
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
        // L3 review (phase 2): `discover` used to wrap `parse`'s
        // already-formatted error (which already carries "configuration
        // error: ") instead of its message, doubling the prefix.
        let root = fixture_dir("discover-double-prefix");
        let commands_dir = root.join("commands");
        std::fs::create_dir_all(&commands_dir).expect("failed to create commands directory");
        std::fs::write(commands_dir.join("broken.md"), "no frontmatter\n")
            .expect("failed to write fixture");

        let err = discover(&root).expect_err("missing frontmatter should fail");
        let msg = err.to_string();
        assert!(msg.contains("broken.md"));
    }

    // -- reserved names (phase 5, point 2 of the shared contract) --------------

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
    fn discover_rejects_models_reserved_name() {
        let root = fixture_dir("reserved-models");
        write_command(&root, "models", "qwen-fast", "prompt");

        let err = discover(&root).expect_err("\"models\" should be rejected as a reserved name");

        let msg = err.to_string();
        assert!(msg.contains("models.md"), "got: {msg}");
        assert!(msg.contains("\"models\""), "got: {msg}");
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
        // Point 2 of the shared contract: the rejection only applies to
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
        // doc). This test still verifies the usual shadowing guarantee
        // (L3 review, phase 2): the general file, shadowed, is NEVER
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

    // -- [args.*] (phase 3) -----------------------------------------------------

    #[test]
    fn parses_full_arg_spec() {
        let source = "+++\nmodel = \"qwen-fast\"\n\n[args.language]\nshort = \"l\"\n\
                       required = true\ndescription = \"Target language\"\n+++\nHello {{ args.language }}\n";
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
        let source = "+++\nmodel = \"qwen-fast\"\n\n[args.language]\nrequired = true\n+++\nHello {{ args.language }}\n";
        let spec =
            parse(source, vec!["translate".to_string()], &test_scope_root()).expect("should parse");

        assert_eq!(spec.args.get("language").expect("present").short, None);
    }

    #[test]
    fn arg_short_multi_character_is_config_error() {
        let source = "+++\nmodel = \"qwen-fast\"\n\n[args.language]\nshort = \"lang\"\n+++\nHello {{ args.language }}\n";
        let err = parse(source, vec!["translate".to_string()], &test_scope_root())
            .expect_err("a multi-character short should fail");

        assert!(matches!(err, crate::Error::Config(_)));
        let message = err.to_string();
        assert!(message.contains("language"), "got: {message}");
        assert!(message.contains("lang"), "got: {message}");
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
            "+++\nmodel = \"qwen-fast\"\n\n[args.x]\nshort = \"-\"\n+++\nHello {{ args.x }}\n";
        let err = parse(source, vec!["x".to_string()], &test_scope_root())
            .expect_err("a \"-\" short should fail at load time, never panic");

        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains('x'));
    }

    #[test]
    fn arg_required_defaults_to_false() {
        // The prompt does NOT reference `{{ args.language }}`: since fix
        // 2 (L3 review), an argument referenced by the prompt must be
        // `required = true` (see
        // `referenced_arg_with_required_false_is_a_load_error` below).
        // This test targets only `required`'s default value, so the
        // prompt cannot reference the argument without changing what it
        // tests.
        let source = "+++\nmodel = \"qwen-fast\"\n\n[args.language]\nshort = \"l\"\n+++\nHello {{ input }}\n";
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
        // reference, exactly the defect the L3 reviews' architecture rule
        // targets.
        let source = "+++\nmodel = \"qwen-fast\"\n\n[args.\"café\"]\nshort = \"c\"\n+++\nHello {{ input }}\n";
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
        let source = "+++\nmodel = \"qwen-fast\"\n\n[args.\"foo.bar\"]\n+++\nHello {{ input }}\n";
        let err = parse(source, vec!["x".to_string()], &test_scope_root())
            .expect_err("an argument name containing a dot should fail at load time");

        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("foo.bar"));
    }

    #[test]
    fn arg_named_help_is_config_error() {
        let source = "+++\nmodel = \"qwen-fast\"\n\n[args.help]\nshort = \"h\"\n+++\nHello {{ args.help }}\n";
        let err = parse(source, vec!["x".to_string()], &test_scope_root())
            .expect_err("an argument named \"help\" should fail");

        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("help"));
    }

    #[test]
    fn arg_named_file_is_config_error() {
        // `FILE` is the id of the positional argument `lib.rs` adds for
        // commands accepting a file as input (phase 3): an argument
        // declared with the same name would collide (duplicate clap id)
        // and must therefore be rejected here, at load time.
        let source = "+++\nmodel = \"qwen-fast\"\n\n[args.FILE]\nshort = \"f\"\n+++\nHello {{ args.FILE }}\n";
        let err = parse(source, vec!["x".to_string()], &test_scope_root())
            .expect_err("an argument named \"FILE\" should fail");

        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("FILE"));
    }

    #[test]
    fn two_args_sharing_the_same_short_letter_is_config_error() {
        let source = "+++\nmodel = \"qwen-fast\"\n\n[args.alpha]\nshort = \"x\"\n\n[args.beta]\n\
                       short = \"x\"\n+++\nHello {{ args.alpha }} {{ args.beta }}\n";
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
        let source = "+++\nmodel = \"qwen-fast\"\n+++\nHello {{ args.language }}\n";
        let err = parse(source, vec!["translate".to_string()], &test_scope_root())
            .expect_err("an undeclared args.* placeholder should fail at parse time");

        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("language"));
    }

    #[test]
    fn declared_but_unreferenced_arg_is_not_an_error() {
        let source =
            "+++\nmodel = \"qwen-fast\"\n\n[args.unused]\nshort = \"u\"\n+++\nHello {{ input }}\n";
        let spec = parse(source, vec!["x".to_string()], &test_scope_root()).expect("should parse");

        assert_eq!(spec.args.len(), 1);
        assert!(spec.args.contains_key("unused"));
    }

    #[test]
    fn command_without_args_section_has_empty_args() {
        let source = "+++\nmodel = \"qwen-fast\"\n+++\nprompt\n";
        let spec = parse(source, vec!["x".to_string()], &test_scope_root()).expect("should parse");

        assert!(spec.args.is_empty());
    }

    #[test]
    fn existing_commands_without_args_section_still_parse_non_regression() {
        let source = "+++\ndescription = \"Classify\"\nmodel = \"qwen-fast\"\n\n[input]\n\
                       mode = \"stdin_or_file\"\n+++\nHello {{ input }}\n";
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
        // Same requirement as the L3 review (phase 2) for a broken
        // frontmatter, applied to the new failure class introduced in
        // phase 3: an undeclared {{ args.unknown }} placeholder is now
        // detected by `parse` itself. A general scope carrying this
        // defect but fully shadowed by a valid local override must still
        // never be opened nor validated.
        let general = fixture_dir("scopes-broken-placeholder-masked-general");
        let commands_dir = general.join("commands");
        std::fs::create_dir_all(&commands_dir).expect("failed to create commands directory");
        std::fs::write(
            commands_dir.join("translate.md"),
            "+++\nmodel = \"qwen-general\"\n+++\nHello {{ args.unknown }}\n",
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
        // (phase 1) and the non-shadowed counterpart of
        // `discover_scopes_broken_placeholder_fully_masked_by_local_scope_...`:
        // this is the failure mode targeted by the L3 reviews' architecture
        // rule ("a key read then silently ignored is a defect") applied to
        // phase 3 — a misspelled placeholder ({{ args.langauge }} instead
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
            "+++\nmodel = \"qwen-fast\"\n\n[args.language]\nshort = \"l\"\n+++\n\
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

    // -- L3 review, fix 2: required = false + referenced by the prompt --

    #[test]
    fn referenced_arg_with_required_false_is_a_load_error_naming_the_file() {
        // Exact reproduction of case 1 from the L3 review: `[args.tone]`
        // with `required = false`, referenced by `{{ args.tone }}`. Must
        // fail AT LOAD TIME, not only at render time — and `discover`
        // must name the offending file (same contract as
        // `discover_reports_faulty_file_path_and_declared_args_on_misspelled_placeholder`).
        let root = fixture_dir("referenced-arg-optional-is-load-error");
        let commands_dir = root.join("commands");
        std::fs::create_dir_all(&commands_dir).expect("failed to create commands directory");
        std::fs::write(
            commands_dir.join("optarg.md"),
            "+++\nmodel = \"qwen-fast\"\n\n[args.tone]\nrequired = false\n+++\n\
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
        let source = "+++\nmodel = \"qwen-fast\"\n\n[args.tone]\nrequired = true\n+++\ntone {{ args.tone }}: {{ input }}\n";
        let spec =
            parse(source, vec!["optarg".to_string()], &test_scope_root()).expect("should parse");

        assert!(spec.args.get("tone").expect("present").required);
    }

    #[test]
    fn declared_but_unreferenced_arg_with_required_false_stays_valid() {
        // Non-regression explicitly requested by the L3 review: fix 2
        // must only reject arguments REFERENCED by the prompt. A declared
        // but unreferenced argument remains a valid, optional CLI
        // argument (see also `declared_but_unreferenced_arg_is_not_an_error`,
        // which doesn't set `required` explicitly).
        let source = "+++\nmodel = \"qwen-fast\"\n\n[args.unused]\nrequired = false\n+++\nHello {{ input }}\n";
        let spec = parse(source, vec!["x".to_string()], &test_scope_root()).expect("should parse");

        assert!(!spec.args.get("unused").expect("present").required);
    }

    // -- L3 review, fix 3: unknown frontmatter keys -----------------

    #[test]
    fn unknown_root_level_frontmatter_key_is_config_error() {
        let source = "+++\nmodel = \"qwen-fast\"\ndescripton = \"typo\"\n+++\nprompt\n";
        let err = parse(source, vec!["x".to_string()], &test_scope_root())
            .expect_err("an unknown root key should fail, not be silently ignored");

        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn unknown_input_section_key_is_config_error() {
        let source = "+++\nmodel = \"qwen-fast\"\n\n[input]\nmoed = \"stdin\"\n+++\nprompt\n";
        let err = parse(source, vec!["x".to_string()], &test_scope_root()).expect_err(
            "an unknown key under [input] should fail, not silently fall back to the \
                          default mode",
        );

        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn output_section_is_accepted_and_now_interpreted() {
        // Follow-up to the L3 review (fix 3): `[output]` already exists
        // in the versioned fixture `.npu/commands/commit-message.md`
        // (format = "text", max_lines = 1). Phase 4 finally gives it
        // meaning: the three keys must now be EFFECTIVE, not just
        // accepted by `deny_unknown_fields`.
        let source = "+++\nmodel = \"qwen-fast\"\n\n[output]\nformat = \"text\"\nmax_lines = 1\n\
                       +++\nprompt\n";
        let spec = parse(source, vec!["x".to_string()], &test_scope_root())
            .expect("a valid [output] section should parse");

        assert_eq!(spec.prompt, "prompt");
        assert_eq!(spec.output.format, crate::output::Format::Text);
        assert_eq!(spec.output.max_lines, Some(1));
        assert_eq!(spec.output.schema, None);
    }

    // -- [output] (phase 4) ----------------------------------------------------

    #[test]
    fn output_section_absent_yields_default_output_spec() {
        let source = "+++\nmodel = \"qwen-fast\"\n+++\nprompt\n";
        let spec = parse(source, vec!["x".to_string()], &test_scope_root())
            .expect("the absence of [output] should always parse (phase 4, task rule 1)");

        assert_eq!(spec.output.format, crate::output::Format::Text);
        assert_eq!(spec.output.schema, None);
        assert_eq!(spec.output.max_lines, None);
    }

    #[test]
    fn output_json_with_schema_resolves_relative_to_scope_root_not_cwd_nor_command_file() {
        // The easiest point to get wrong in phase 4 (rule 3 of the shared
        // contract): `schemas/` is a SIBLING directory of `commands/`,
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

        let source = "+++\nmodel = \"qwen-fast\"\n\n[output]\nformat = \"json\"\n\
                       schema = \"schemas/classification.json\"\n+++\nprompt\n";
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
    fn output_json_schema_absolute_path_is_used_as_is_not_joined_with_scope_root() {
        // Uncovered branch of `resolve_schema_path` (L1/L2 review of
        // phase 4): an ABSOLUTE path in `[output].schema` must resolve to
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
            "+++\nmodel = \"qwen-fast\"\n\n[output]\nformat = \"json\"\nschema = \"{}\"\n\
             +++\nprompt\n",
            schema_path.display()
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
        // Rule 2 of the shared contract: `format = "json"` WITHOUT
        // `schema` is explicitly allowed, only checking that the output
        // is well-formed JSON.
        let source = "+++\nmodel = \"qwen-fast\"\n\n[output]\nformat = \"json\"\n+++\nprompt\n";
        let spec = parse(source, vec!["x".to_string()], &test_scope_root())
            .expect("format = json without schema should parse");

        assert_eq!(spec.output.format, crate::output::Format::Json);
        assert_eq!(spec.output.schema, None);
    }

    #[test]
    fn output_schema_with_text_format_is_config_error() {
        // Forbidden combination (rule 2 of the shared contract): a schema
        // means nothing on plain text.
        let source = "+++\nmodel = \"qwen-fast\"\n\n[output]\nformat = \"text\"\n\
                       schema = \"schemas/x.json\"\n+++\nprompt\n";
        let err = parse(source, vec!["x".to_string()], &test_scope_root())
            .expect_err("schema with format = text should be rejected at load time");

        assert!(matches!(err, crate::Error::Config(_)));
        let message = err.to_string();
        assert!(message.contains("schema"), "got: {message}");
        assert!(message.contains("text"), "got: {message}");
    }

    #[test]
    fn output_max_lines_with_json_format_is_config_error() {
        // Symmetric forbidden combination (rule 2 of the shared
        // contract): max_lines only applies to text.
        let source = "+++\nmodel = \"qwen-fast\"\n\n[output]\nformat = \"json\"\nmax_lines = 3\n\
             +++\nprompt\n";
        let err = parse(source, vec!["x".to_string()], &test_scope_root())
            .expect_err("max_lines with format = json should be rejected at load time");

        assert!(matches!(err, crate::Error::Config(_)));
        let message = err.to_string();
        assert!(message.contains("max_lines"), "got: {message}");
        assert!(message.contains("json"), "got: {message}");
    }

    #[test]
    fn output_schema_pointing_to_missing_file_resolves_lazily_at_load() {
        // L3 review, fix 1: `resolve_schema_path` is now PURELY
        // SYNTACTIC — a declared but disk-absent schema must no longer
        // make loading (`parse`) fail, aligned with the schema's lazy
        // compilation (see `resolve_schema_path`'s doc and `output.rs`'s).
        // The resolved path must nonetheless stay correct: it's
        // `output::compile_schema`, at the command's actual execution,
        // that discovers the absence (see the next test,
        // `output_schema_missing_file_error_names_the_command_file_via_discover`).
        let root = fixture_dir("output-schema-missing");
        let source = "+++\nmodel = \"qwen-fast\"\n\n[output]\nformat = \"json\"\n\
                       schema = \"schemas/does-not-exist.json\"\n+++\nprompt\n";
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
        // L3 review, fix 1: locks in the NEW contract, not the old one.
        // Loading (`discover`) now succeeds even if the declared schema
        // is absent (lazy resolution, see `resolve_schema_path`) —
        // `--help` and every other command in the same scope remain
        // usable. The error occurs at USE, when the command requesting
        // this schema is actually invoked (`output::finalize`, which
        // delegates to `compile_schema`), and must name BOTH paths: the
        // schema's resolved path AND the command file requesting it —
        // same requirement as for a broken frontmatter or an unknown
        // placeholder (the L3 reviews' architecture rule for phases 1 to
        // 3).
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
            message.contains(
                root.join("schemas")
                    .join("absent.json")
                    .to_string_lossy()
                    .as_ref()
            ),
            "the message must also cite the schema's resolved path, got: {message}"
        );
    }

    #[test]
    fn unknown_output_section_key_is_config_error() {
        // Same architecture rule as the rest of the frontmatter
        // (`deny_unknown_fields`, L3 reviews of phases 1 to 3), now
        // applied to `[output]`.
        let source = "+++\nmodel = \"qwen-fast\"\n\n[output]\nformt = \"json\"\n+++\nprompt\n";
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
            format!("+++\nmodel = \"{model}\"\n\n[output]\n{output_toml}\n+++\n{prompt}\n"),
        )
        .expect("failed to write fixture");
    }

    #[test]
    fn real_commit_message_fixture_output_section_is_now_effective() {
        // This is the exact debt this phase repays: the versioned
        // fixture `.npu/commands/commit-message.md` has declared
        // `[output]` (format = "text", max_lines = 1) since phase 3,
        // never honored until now. It must now parse AND yield
        // max_lines = Some(1).
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
            "the L3 review's debt (fix 3) is repaid: max_lines must be effective, not just \
             accepted"
        );
        assert_eq!(commit_message.output.schema, None);
    }
}

//! Generic execution engine for local AI commands.
//!
//! All the logic lives here: the binary (`main.rs`) is only a shell.
//! This is what makes the pipeline testable from `tests/`, which can only
//! import a library target.

pub mod backend;
pub mod builtin;
pub mod command;
pub mod config;
pub mod error;
pub mod input;
pub mod log;
pub mod output;
pub mod prompt;
pub mod scope;

pub use error::{Error, Result};

/// A node of the command tree built from the discovered `CommandSpec`s.
/// `spec` is set on a leaf command; an intermediate node (e.g. `git` before
/// `git review`) only has children.
struct CommandNode<'a> {
    spec: Option<&'a command::CommandSpec>,
    children: std::collections::BTreeMap<String, CommandNode<'a>>,
}

impl CommandNode<'_> {
    fn new() -> Self {
        CommandNode {
            spec: None,
            children: std::collections::BTreeMap::new(),
        }
    }
}

/// Groups the `CommandSpec`s into a tree by path prefix: two commands
/// sharing a leading segment (`git review`, `git commit`) merge under the
/// same intermediate node `git`.
fn build_command_tree(specs: &[command::CommandSpec]) -> CommandNode<'_> {
    let mut root = CommandNode::new();
    for spec in specs {
        let mut node = &mut root;
        for segment in &spec.path {
            node = node
                .children
                .entry(segment.clone())
                .or_insert_with(CommandNode::new);
        }
        node.spec = Some(spec);
    }
    root
}

/// Builds the `clap::Arg` corresponding to a declared argument
/// (`[args.*]`): `.long(name)`, `.short(letter)` if present,
/// `.required(required)`, `.help(description)` if non-empty, `.value_name(name
/// in UPPERCASE)` and the single-value action (`ArgAction::Set`, an
/// argument expects a single value — no multi-values, no boolean flag,
/// cf. §11/§25). The argument's id is `name` itself: this is what
/// `collect_arg_values` reads back from the leaf command's `ArgMatches`.
fn build_declared_arg(name: &str, arg_spec: &command::ArgSpec) -> clap::Arg {
    let mut arg = clap::Arg::new(name.to_string())
        .long(name.to_string())
        .required(arg_spec.required)
        .value_name(name.to_uppercase())
        .action(clap::ArgAction::Set);

    if let Some(short) = arg_spec.short {
        arg = arg.short(short);
    }
    if !arg_spec.description.is_empty() {
        arg = arg.help(arg_spec.description.clone());
    }

    arg
}

/// Recursively builds the `clap` subtree corresponding to `node`, named
/// `name`. A leaf command receives its description, an optional `FILE`
/// positional argument if it accepts a file as input, then a `clap::Arg`
/// per declared argument (`[args.*]`, phase 3 — cf. [`build_declared_arg`]).
/// Iterating over `spec.args` (a `BTreeMap`) is sorted by name, so the order
/// of arguments in `--help` is deterministic regardless of the order the
/// TOML frontmatter was written in. `command::parse` already reserves the
/// name `FILE` (cf. `RESERVED_ARG_NAMES`): no declared argument can
/// therefore collide with the positional argument added here. An
/// intermediate node (with no `CommandSpec` of its own) remains usable on
/// its own: it then shows its help instead of failing silently.
fn build_clap_node(name: &str, node: &CommandNode<'_>) -> clap::Command {
    let mut cmd = clap::Command::new(name.to_string());

    match node.spec {
        Some(spec) => {
            cmd = cmd.about(spec.description.clone());
            if matches!(
                spec.input,
                command::InputMode::File | command::InputMode::StdinOrFile
            ) {
                cmd = cmd.arg(clap::Arg::new("FILE").required(false));
            }
            for (arg_name, arg_spec) in &spec.args {
                cmd = cmd.arg(build_declared_arg(arg_name, arg_spec));
            }
        }
        None => {
            cmd = cmd.arg_required_else_help(true);
        }
    }

    for (child_name, child_node) in &node.children {
        cmd = cmd.subcommand(build_clap_node(child_name, child_node));
    }

    cmd
}

/// The `--verbose <LEVEL>` argument, declared once on the root and marked
/// `global`, so it is accepted after any subcommand (`npu classify
/// --verbose info`) without being redeclared on each of them.
///
/// The accepted values come from `log::Level::NAMES`: the CLI cannot offer a
/// level the logger does not know, nor the reverse. `command.rs` reserves
/// the name `verbose` and the short letter `v` for the same reason it
/// reserves `help`/`h` — a command declaring them would collide with this
/// argument.
fn verbose_arg() -> clap::Arg {
    clap::Arg::new("verbose")
        .long("verbose")
        .short('v')
        .global(true)
        .value_name("LEVEL")
        .value_parser(log::Level::NAMES)
        .default_value(log::Level::DEFAULT)
        .help("Diagnostic verbosity on stderr; stdout always carries the result only")
}

/// Builds the complete `clap` tree (builder API) from the discovered
/// commands. Contains ONLY the business
/// commands: the built-ins (`doctor`/`models`/`serve`/`stop`/`status`/`logs`/`describe`) are added
/// separately by [`add_builtins`], unconditionally — this function remains
/// usable with an empty `specs` (degraded mode, cf. `run`).
fn build_cli(specs: &[command::CommandSpec]) -> clap::Command {
    let tree = build_command_tree(specs);
    let mut root = clap::Command::new("npu")
        .arg_required_else_help(true)
        .arg(verbose_arg());
    for (name, node) in &tree.children {
        root = root.subcommand(build_clap_node(name, node));
    }
    root
}

/// Adds the CLI's built-ins (`doctor`, `models`, `serve`, `stop`, `status`,
/// `logs`, `describe`) to the
/// tree already built from the discovered business commands.
///
/// Called UNCONDITIONALLY by `run`, including when loading the
/// configuration has failed (degraded mode, point 1 of the shared
/// contract): a broken configuration must NEVER deprive `--help` of
/// `doctor`, since `doctor` is precisely the tool meant to diagnose its
/// cause. `command::reject_reserved_path` already guarantees, upstream at
/// discovery time, that no business command can carry one of these names as
/// its first path segment: these `subcommand()` calls can therefore never
/// collide with the ones added by [`build_cli`].
fn add_builtins(cli: clap::Command) -> clap::Command {
    // Help text in English: it is displayed next to the configured
    // commands' `description`, and the repository's documentation is in
    // English.
    cli.subcommand(clap::Command::new("doctor").about(
        "Check the runtime environment: configuration, backend reachability, declared \
         output schemas",
    ))
    .subcommand(clap::Command::new("models").about("List configured models"))
    .subcommand(
        clap::Command::new("serve")
            .about("Start the container runtime of the backend a model points at")
            .arg(
                clap::Arg::new("MODEL")
                    .required(true)
                    .help("Identifier of the model to serve (e.g. \"qwen-fast\")"),
            ),
    )
    .subcommand(
        clap::Command::new("stop")
            .about("Stop and remove the container started for a model's backend")
            .arg(
                clap::Arg::new("MODEL")
                    .required(true)
                    .help("Identifier of the model whose runtime is stopped"),
            ),
    )
    .subcommand(
        clap::Command::new("status").about("Report the state of every containerized backend"),
    )
    .subcommand(
        clap::Command::new("logs")
            .about("Stream the logs of the container started for a model's backend")
            .arg(
                clap::Arg::new("MODEL")
                    .required(true)
                    .help("Identifier of the model whose runtime is read"),
            )
            .arg(
                clap::Arg::new("follow")
                    .long("follow")
                    .short('f')
                    .action(clap::ArgAction::SetTrue)
                    .help("Keep streaming as new lines arrive"),
            ),
    )
    .subcommand(
        clap::Command::new("describe")
            .about("Describe a dynamically configured command, as JSON")
            .arg(
                clap::Arg::new("COMMAND")
                    .required(true)
                    .help("Path of the command to describe (e.g. \"classify\" or \"git/review\")"),
            ),
    )
}

/// Reconstructs the path of the selected command by walking down the chain
/// of subcommands `clap` resolved, and returns the leaf command's
/// `ArgMatches` along with the path walked.
fn selected_path(matches: &clap::ArgMatches) -> (Vec<String>, &clap::ArgMatches) {
    let mut path = Vec::new();
    let mut current = matches;
    while let Some((name, sub_matches)) = current.subcommand() {
        path.push(name.to_string());
        current = sub_matches;
    }
    (path, current)
}

/// Finds, among `specs`, the command whose path joined by `/` equals `key`.
/// Returns an `Error::Config` listing the available commands if none match
/// (same style as `config::Config::resolve` and `backend::chat` for unknown
/// identifiers).
fn find_command<'a>(
    specs: &'a [command::CommandSpec],
    key: &str,
) -> Result<&'a command::CommandSpec> {
    specs
        .iter()
        .find(|spec| spec.path.join("/") == key)
        .ok_or_else(|| {
            Error::Config(format!(
                "unknown command: \"{key}\" (available commands: {})",
                error::format_available(specs.iter().map(|s| s.path.join("/")))
            ))
        })
}

/// Collects, for the leaf command `spec`, the values of its declared
/// arguments (`[args.*]`) from the corresponding `ArgMatches`, into the
/// `BTreeMap<String, String>` expected by `prompt::render`.
///
/// A declared argument absent from `leaf_matches` (not required and not
/// supplied on the command line) is simply omitted from the map: phase 3
/// does not introduce a default value. If the
/// prompt still references this argument via `{{ args.NAME }}`,
/// `prompt::render` fails with an `Error::Config` naming the argument (rule
/// 6 of the shared contract) rather than substituting an unrequested empty
/// string.
fn collect_arg_values(
    spec: &command::CommandSpec,
    leaf_matches: &clap::ArgMatches,
) -> std::collections::BTreeMap<String, String> {
    spec.args
        .keys()
        .filter_map(|name| {
            leaf_matches
                .get_one::<String>(name)
                .map(|value| (name.clone(), value.clone()))
        })
        .collect()
}

/// Executes the pipeline of an already-resolved BUSINESS command (`spec`),
/// with the loaded configuration (`config`, necessarily `Ok` at this point:
/// `run` has propagated any load error before reaching this function) and
/// the `ArgMatches` of the selected leaf command.
///
/// Extracted from `run` as-is (phase 5, degraded-mode wiring): the only
/// change from the version before this phase is where this sequence is
/// called from, never its content nor its internal order.
///
/// INVARIANT (L3 review, fix 1, phase 3) — preserved IDENTICALLY: nothing
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
fn execute_business_command(
    spec: &command::CommandSpec,
    config: &config::Config,
    leaf_matches: &clap::ArgMatches,
    logger: log::Logger,
) -> Result<()> {
    let (model, backend) = config.resolve(&spec.model)?;
    logger.info(&format!(
        "command \"{}\" -> model \"{}\" (backend \"{}\", operation \"{}\") from {}",
        spec.path.join("/"),
        model.id,
        backend.id,
        model.operation,
        spec.file.display()
    ));

    let args = collect_arg_values(spec, leaf_matches);
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

    prompt::preflight(&spec.prompt, &args, &env)?;

    // The `FILE` argument is only declared (cf. `build_clap_node`) for the
    // modes that accept a file: reproducing the same condition here avoids
    // calling `get_one` on an absent id (panic) and keeps the two spots in
    // sync if a later phase changes one without the other.
    let file_arg = match spec.input {
        command::InputMode::File | command::InputMode::StdinOrFile => leaf_matches
            .get_one::<String>("FILE")
            .map(std::path::Path::new),
        command::InputMode::Stdin => None,
    };
    let input_text = input::resolve(&spec.input, file_arg)?;
    logger.info(&format!(
        "input: {} characters read from {}",
        input_text.chars().count(),
        match file_arg {
            Some(path) => path.display().to_string(),
            None => "stdin".to_string(),
        }
    ));

    let prompt = prompt::render(&spec.prompt, &input_text, &args, &env)?;
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
    // execution failure, not something to recover from (§15, out of scope
    // for this phase).
    let raw_output = backend::chat(backend, model, &prompt, logger)?;
    let output = output::finalize(&spec.output, &raw_output, &spec.file)?;
    logger.info(&format!(
        "output contract honoured ({}): {} characters written to stdout",
        spec.output.format.as_str(),
        output.chars().count()
    ));

    println!("{output}");

    Ok(())
}

/// Entry point of the library, called by `main`.
///
/// Pipeline: resolving the scope roots
/// (`scope::roots()`, from the most general to the most local — `/etc/npu`,
/// then `$XDG_CONFIG_HOME/npu` or `$HOME/.config/npu`, then `./.npu`),
/// loading and merging the configuration across these roots
/// (`config::load_scopes`), discovering and merging the commands
/// (`command::discover_scopes`), building the `clap` tree, resolving the
/// selected command, the model, the input, rendering the prompt, calling
/// the backend, writing the result to stdout. Merging across scopes is a
/// replacement by identifier (backends/models) or by full path (commands),
/// never a field-by-field merge; a root
/// absent from disk is simply not taken into account.
///
/// **Degraded mode (phase 5, point 1 of the shared contract — paying off
/// the debt tracked since phase 1).** Loading (`config::load_scopes` THEN
/// `command::discover_scopes`) can fail (malformed TOML, broken
/// frontmatter, unknown reference). Its error is now KEPT
/// (`loaded: Result<(Config, Vec<CommandSpec>)>`) rather than immediately
/// propagated via `?`: the `clap` tree is built UNCONDITIONALLY with the
/// built-ins ([`add_builtins`]), and with the discovered business
/// commands ONLY if loading succeeded (otherwise `build_cli(&[])`).
/// Consequences:
/// - `--help` ALWAYS works, even with a broken configuration;
/// - a line on STDERR reports the load failure and points to `npu doctor`
///   — emitted BEFORE `cli.get_matches()`, because `--help` exits via
///   `clap`'s internal `std::process::exit` without ever returning through
///   the rest of this function (cf. `tests/clap_error_stdout_purity.rs`);
/// - `npu doctor` ALWAYS runs (before any other branch) and reports the
///   kept load error as a failed check (a) (cf. `builtin::doctor`);
/// - any OTHER invocation (business command, `models`, `serve`, `stop`,
///   `status`, `logs`, `describe`)
///   propagates the kept load error via `loaded?`, exit code 2 — unchanged
///   from before this phase.
///
/// When loading SUCCEEDS, the behavior is identical to before this phase,
/// invariant included (cf. [`execute_business_command`]'s doc).
///
/// Exit code carried by the return type: `Ok(0)` (ordinary success,
/// including a business command), `Ok(code)` for `code != 0` is the exit
/// code of a REPORT (`npu doctor` — cf. `builtin::doctor_exit_code` — this
/// is not an engine failure, just a diagnostic that is not entirely green),
/// and `Err` remains reserved for pipeline failures (`Error::exit_code`).
/// `main` translates the three cases; it is `run` that writes `doctor`'s
/// report (below, via `println!`, so already on stdout before returning the
/// code) — never `main` itself, so that stdout stays reserved for the
/// RESULT produced by this library, as everywhere else in this module.
pub fn run() -> Result<i32> {
    // Read from the RAW command line: this logger must exist BEFORE
    // `get_matches()`, since the degraded-mode warning below is emitted
    // before parsing (cf. `log::level_from_args`). `clap` re-reads the same
    // flag afterwards and is the one that rejects an invalid value.
    let logger = log::Logger::new(log::level_from_args(std::env::args()));

    let roots = scope::roots();
    logger.info(&format!(
        "scopes: {}",
        error::format_available(roots.iter().map(|root| root.display().to_string()))
    ));

    let loaded: Result<(config::Config, Vec<command::CommandSpec>)> = config::load_scopes(&roots)
        .and_then(|config| command::discover_scopes(&roots).map(|specs| (config, specs)));

    if let Err(err) = &loaded {
        // MUST precede `cli.get_matches()`: see this function's doc. Never
        // on stdout (contract rule: stdout is reserved for the result) —
        // this line is an ENGINE diagnostic, not a command result.
        logger.warn(&format!(
            "invalid configuration ({err}); run \"npu doctor\" for details on the failed checks"
        ));
    }

    let specs_for_cli: &[command::CommandSpec] = match &loaded {
        Ok((_, specs)) => specs,
        Err(_) => &[],
    };
    let cli = add_builtins(build_cli(specs_for_cli));
    let matches = cli.get_matches();

    let (path, leaf_matches) = selected_path(&matches);
    let builtin_name = path.first().map(String::as_str);

    if builtin_name == Some("doctor") {
        // `doctor` ALWAYS runs, whether loading succeeded or failed: this
        // is precisely its point in degraded mode (point 3 of the shared
        // contract). `probe` is the REAL probe (`builtin::tcp_probe`),
        // never a stub — `builtin.rs`'s tests inject their own directly on
        // `builtin::doctor`, this function here only wires in the real
        // probe.
        let (config_ref, specs_ref, load_error_ref) = match &loaded {
            Ok((config, specs)) => (Some(config), Some(specs.as_slice()), None),
            Err(err) => (None, None, Some(err)),
        };
        let checks = builtin::doctor(
            config_ref,
            specs_ref,
            load_error_ref,
            &builtin::tcp_probe,
            &builtin::docker_probe,
        );
        println!("{}", builtin::format_doctor(&checks));
        return Ok(builtin::doctor_exit_code(&checks));
    }

    // Any OTHER branch (business command, `models`, the lifecycle commands,
    // `describe`) requires a
    // successfully loaded configuration: propagates the error KEPT above,
    // code 2, exactly as before this phase (point 1 of the shared
    // contract).
    let (config, specs) = loaded?;

    if builtin_name == Some("models") {
        println!("{}", builtin::format_models(&config));
        return Ok(0);
    }

    if builtin_name == Some("serve") {
        // `MODEL` is declared `.required(true)` by `add_builtins`: clap has
        // already rejected the invocation if it is absent.
        let model_id = leaf_matches
            .get_one::<String>("MODEL")
            .map(String::as_str)
            .unwrap_or_default();
        let env = |name: &str| std::env::var(name).ok();
        // The container identifier IS this command's result: stdout, like
        // every other built-in's report. Docker's own output goes to stderr
        // (cf. `builtin::docker_runner`).
        println!(
            "{}",
            builtin::serve(&config, model_id, &env, &builtin::docker_runner)?
        );
        return Ok(0);
    }

    if builtin_name == Some("stop") {
        let model_id = leaf_matches
            .get_one::<String>("MODEL")
            .map(String::as_str)
            .unwrap_or_default();
        logger.info(&format!("stopping the runtime of model \"{model_id}\""));
        println!(
            "{}",
            builtin::stop(&config, model_id, &builtin::docker_runner)?
        );
        return Ok(0);
    }

    if builtin_name == Some("status") {
        // The report IS the result: stdout, like `doctor` and `models`.
        println!("{}", builtin::status(&config, &builtin::docker_runner)?);
        return Ok(0);
    }

    if builtin_name == Some("logs") {
        let model_id = leaf_matches
            .get_one::<String>("MODEL")
            .map(String::as_str)
            .unwrap_or_default();
        let follow = leaf_matches.get_flag("follow");
        // Nothing is printed here: the container's own streams are the
        // result, and `builtin::docker_streamer` lets them through
        // untouched (cf. its doc).
        builtin::logs(&config, model_id, follow, &builtin::docker_streamer)?;
        return Ok(0);
    }

    if builtin_name == Some("describe") {
        // `COMMAND` is declared `.required(true)` by `add_builtins`: clap
        // has already rejected the invocation before `get_matches()` if
        // the argument is absent, so `leaf_matches` always carries it at
        // this point.
        let name = leaf_matches
            .get_one::<String>("COMMAND")
            .map(String::as_str)
            .unwrap_or_default();
        let spec = find_command(&specs, name)?;
        println!("{}", builtin::describe(spec)?);
        return Ok(0);
    }

    let key = path.join("/");
    let spec = find_command(&specs, &key)?;

    execute_business_command(spec, &config, leaf_matches, logger)?;

    Ok(0)
}

#[cfg(test)]
#[allow(clippy::expect_used)] // allowed in tests (cf. Cargo.toml [lints.clippy]).
mod tests {
    use super::*;
    use command::{CommandSpec, InputMode};

    fn spec(path: &[&str], input: InputMode) -> CommandSpec {
        spec_with_args(path, input, std::collections::BTreeMap::new())
    }

    fn spec_with_args(
        path: &[&str],
        input: InputMode,
        args: std::collections::BTreeMap<String, command::ArgSpec>,
    ) -> CommandSpec {
        CommandSpec {
            path: path.iter().map(ToString::to_string).collect(),
            description: format!("desc {}", path.join("/")),
            model: "qwen-fast".to_string(),
            input,
            prompt: "{{ input }}".to_string(),
            args,
            // Phase 4 (`command::CommandSpec.output`, field added by the
            // shared API contract): these `lib.rs` tests bear on the
            // construction of the `clap` tree, never on the output
            // contract — `OutputSpec::default()` (text format, no schema,
            // no limit) is neutral for them.
            output: output::OutputSpec::default(),
            // Neutral for the same reasons as `output` above: these tests
            // never bear on the output contract nor on naming the command
            // file in a schema error.
            file: std::path::PathBuf::new(),
        }
    }

    fn arg_spec(short: Option<char>, required: bool, description: &str) -> command::ArgSpec {
        command::ArgSpec {
            short,
            required,
            description: description.to_string(),
        }
    }

    #[test]
    fn build_cli_merges_commands_sharing_a_prefix() {
        let specs = vec![
            spec(&["commit-message"], InputMode::Stdin),
            spec(&["git", "review"], InputMode::StdinOrFile),
            spec(&["git", "commit"], InputMode::File),
        ];

        let cli = build_cli(&specs);

        let top_names: Vec<&str> = cli.get_subcommands().map(clap::Command::get_name).collect();
        assert!(top_names.contains(&"commit-message"));
        assert!(top_names.contains(&"git"));

        let git = cli
            .find_subcommand("git")
            .expect("git must exist as a merged subcommand");
        let git_children: Vec<&str> = git.get_subcommands().map(clap::Command::get_name).collect();
        assert!(git_children.contains(&"review"));
        assert!(git_children.contains(&"commit"));
    }

    #[test]
    fn leaf_command_gets_file_arg_only_when_input_accepts_a_file() {
        let specs = vec![
            spec(&["commit-message"], InputMode::Stdin),
            spec(&["classify"], InputMode::StdinOrFile),
            spec(&["summarize"], InputMode::File),
        ];

        let cli = build_cli(&specs);

        let stdin_only = cli
            .find_subcommand("commit-message")
            .expect("commit-message must exist");
        assert!(stdin_only.get_arguments().next().is_none());

        let classify = cli
            .find_subcommand("classify")
            .expect("classify must exist");
        assert!(classify.get_arguments().any(|arg| arg.get_id() == "FILE"));

        let summarize = cli
            .find_subcommand("summarize")
            .expect("summarize must exist");
        assert!(summarize.get_arguments().any(|arg| arg.get_id() == "FILE"));
    }

    #[test]
    fn selected_path_descends_nested_subcommands() {
        let specs = vec![spec(&["git", "review"], InputMode::Stdin)];
        let cli = build_cli(&specs);
        let matches = cli
            .try_get_matches_from(["npu", "git", "review"])
            .expect("the command line must be accepted");

        let (path, leaf) = selected_path(&matches);

        assert_eq!(path, vec!["git".to_string(), "review".to_string()]);
        // The returned `ArgMatches` are indeed the leaf's, not the root's:
        // no subcommand remains to be descended into from `leaf`.
        assert!(leaf.subcommand().is_none());
    }

    #[test]
    fn selected_path_is_empty_when_no_subcommand_is_selected() {
        let specs = vec![spec(&["commit-message"], InputMode::Stdin)];
        let cli = build_cli(&specs);
        // `arg_required_else_help` normally prevents reaching this point
        // without a subcommand in real usage, but `selected_path` must
        // stay correct if it is called anyway on empty root `ArgMatches`.
        let matches = clap::Command::new("npu")
            .try_get_matches_from(["npu"])
            .expect("no subcommand required on this test instance");

        let (path, _leaf) = selected_path(&matches);

        assert!(path.is_empty());
        let _ = cli; // avoids a warning if `cli` is no longer used beyond this point
    }

    #[test]
    fn find_command_returns_matching_spec() {
        let specs = vec![
            spec(&["commit-message"], InputMode::Stdin),
            spec(&["git", "review"], InputMode::Stdin),
        ];
        let found = find_command(&specs, "git/review").expect("git/review must be found");
        assert_eq!(found.path, vec!["git".to_string(), "review".to_string()]);
    }

    #[test]
    fn find_command_unknown_key_lists_available_commands() {
        let specs = vec![
            spec(&["commit-message"], InputMode::Stdin),
            spec(&["git", "review"], InputMode::Stdin),
        ];
        let err =
            find_command(&specs, "does-not-exist").expect_err("the command must not be found");

        assert!(matches!(err, Error::Config(_)));
        let message = err.to_string();
        assert!(message.contains("does-not-exist"));
        assert!(message.contains("commit-message"));
        assert!(message.contains("git/review"));
    }

    #[test]
    fn intermediate_node_without_its_own_spec_is_invocable_alone() {
        // An intermediate node (here `git`, which has no CommandSpec of its
        // own) must remain usable alone: `arg_required_else_help` rather
        // than a silent failure if `npu git` is invoked without a
        // subcommand.
        let specs = vec![spec(&["git", "review"], InputMode::Stdin)];
        let cli = build_cli(&specs);
        let git = cli.find_subcommand("git").expect("git must exist");
        assert!(git.is_arg_required_else_help_set());
    }

    // -- declared CLI arguments (phase 3) --------------------------------------

    #[test]
    fn declared_arg_becomes_clap_arg_with_short_required_help_and_value_name() {
        let mut args = std::collections::BTreeMap::new();
        args.insert(
            "language".to_string(),
            arg_spec(Some('l'), true, "Target language"),
        );
        let specs = vec![spec_with_args(&["translate"], InputMode::StdinOrFile, args)];

        let cli = build_cli(&specs);
        let translate = cli
            .find_subcommand("translate")
            .expect("translate must exist");
        let language = translate
            .get_arguments()
            .find(|arg| arg.get_id() == "language")
            .expect("the 'language' argument must be present");

        assert_eq!(language.get_short(), Some('l'));
        assert!(language.is_required_set());
        assert_eq!(
            language.get_help().map(ToString::to_string).as_deref(),
            Some("Target language")
        );
        assert_eq!(
            language.get_value_names().map(|names| names[0].as_str()),
            Some("LANGUAGE")
        );
    }

    #[test]
    fn declared_arg_without_short_or_description_has_neither() {
        let mut args = std::collections::BTreeMap::new();
        args.insert("unused".to_string(), arg_spec(None, false, ""));
        let specs = vec![spec_with_args(&["x"], InputMode::Stdin, args)];

        let cli = build_cli(&specs);
        let x = cli.find_subcommand("x").expect("x must exist");
        let unused = x
            .get_arguments()
            .find(|arg| arg.get_id() == "unused")
            .expect("the 'unused' argument must be present");

        assert_eq!(unused.get_short(), None);
        assert!(!unused.is_required_set());
        assert!(unused.get_help().is_none());
    }

    #[test]
    fn declared_args_appear_in_deterministic_btreemap_order() {
        let mut args = std::collections::BTreeMap::new();
        args.insert("zebra".to_string(), arg_spec(None, false, ""));
        args.insert("alpha".to_string(), arg_spec(None, false, ""));
        let specs = vec![spec_with_args(&["x"], InputMode::Stdin, args)];

        let cli = build_cli(&specs);
        let x = cli.find_subcommand("x").expect("x must exist");
        let names: Vec<&str> = x
            .get_arguments()
            .map(|arg| arg.get_id().as_str())
            .filter(|id| *id != "help")
            .collect();

        assert_eq!(names, vec!["alpha", "zebra"]);
    }

    #[test]
    fn file_positional_and_declared_args_coexist_without_id_collision() {
        let mut args = std::collections::BTreeMap::new();
        args.insert("language".to_string(), arg_spec(Some('l'), true, ""));
        let specs = vec![spec_with_args(&["translate"], InputMode::StdinOrFile, args)];

        // `build_cli` must not panic (duplicate id): the construction
        // itself is the assertion.
        let cli = build_cli(&specs);
        let translate = cli
            .find_subcommand("translate")
            .expect("translate must exist");
        assert!(translate.get_arguments().any(|arg| arg.get_id() == "FILE"));
        assert!(
            translate
                .get_arguments()
                .any(|arg| arg.get_id() == "language")
        );
    }

    #[test]
    fn collect_arg_values_reads_declared_arg_from_leaf_matches() {
        let mut args = std::collections::BTreeMap::new();
        args.insert("language".to_string(), arg_spec(Some('l'), true, ""));
        let specs = vec![spec_with_args(&["translate"], InputMode::Stdin, args)];
        let cli = build_cli(&specs);
        let matches = cli
            .try_get_matches_from(["npu", "translate", "--language", "french"])
            .expect("the command line must be accepted");
        let (_, leaf) = selected_path(&matches);

        let collected = collect_arg_values(&specs[0], leaf);

        assert_eq!(
            collected.get("language").map(String::as_str),
            Some("french")
        );
    }

    #[test]
    fn collect_arg_values_omits_unset_non_required_arg() {
        let mut args = std::collections::BTreeMap::new();
        args.insert("unused".to_string(), arg_spec(None, false, ""));
        let specs = vec![spec_with_args(&["x"], InputMode::Stdin, args)];
        let cli = build_cli(&specs);
        let matches = cli
            .try_get_matches_from(["npu", "x"])
            .expect("the command line must be accepted without the non-required argument");
        let (_, leaf) = selected_path(&matches);

        let collected = collect_arg_values(&specs[0], leaf);

        assert!(
            !collected.contains_key("unused"),
            "an argument that is not supplied and not required must not appear in the map, \
             no default value (§11/§25)"
        );
    }

    #[test]
    fn missing_required_arg_is_rejected_by_clap() {
        let mut args = std::collections::BTreeMap::new();
        args.insert("language".to_string(), arg_spec(Some('l'), true, ""));
        let specs = vec![spec_with_args(&["translate"], InputMode::Stdin, args)];
        let cli = build_cli(&specs);

        let result = cli.try_get_matches_from(["npu", "translate"]);

        assert!(
            result.is_err(),
            "a missing required argument must be rejected by clap"
        );
    }
}

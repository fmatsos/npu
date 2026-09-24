//! Generic execution engine for local AI commands.
//!
//! All the logic lives here: the binary (`main.rs`) is only a shell.
//! This is what makes the pipeline testable from `tests/`, which can only
//! import a library target.

pub mod backend;
pub mod builtin;
pub mod command;
pub mod config;
pub mod discover;
pub mod error;
pub mod input;
pub mod log;
pub mod output;
pub mod progress;
pub mod prompt;
pub mod runtime;
pub mod scope;
pub mod style;
pub mod tune;
pub mod updater;

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
/// argument expects a single value — no multi-values, no boolean flag).
/// The argument's id is `name` itself: this is what
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
/// per declared argument (`[args.*]` — cf. [`build_declared_arg`]).
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
/// commands: the built-ins (`backend …`, `config …`, `doctor`, `describe`, `update`, `--version`) are added
/// separately by [`add_builtins`], unconditionally — this function remains
/// usable with an empty `specs` (degraded mode, cf. `run`).
fn build_cli(specs: &[command::CommandSpec]) -> clap::Command {
    let tree = build_command_tree(specs);
    let mut root = clap::Command::new("npu")
        .styles(style::clap_styles())
        .arg_required_else_help(true)
        .arg(verbose_arg());
    for (name, node) in &tree.children {
        root = root.subcommand(build_clap_node(name, node));
    }
    root
}

/// `npu model discover`: reads its flags, the host's RAM and NPU, then
/// delegates to [`discover::discover`], whose report is the result.
fn model_discover(
    leaf_matches: &clap::ArgMatches,
    config: std::result::Result<&config::Config, &Error>,
    logger: log::Logger,
) -> Result<String> {
    let npu = leaf_matches.get_flag("npu");
    let engine = match leaf_matches.get_one::<String>("backend") {
        None => npu.then_some(discover::Engine::OpenVino),
        Some(name) => Some(discover_engine(name, config)?),
    };
    if npu && engine != Some(discover::Engine::OpenVino) {
        return Err(Error::Config(
            "--npu runs models through OpenVINO: it cannot be combined with another --backend"
                .to_string(),
        ));
    }
    let words: Vec<&str> = leaf_matches
        .get_many::<String>("QUERY")
        .into_iter()
        .flatten()
        .map(String::as_str)
        .collect();
    let number = |id: &str| usize::from(leaf_matches.get_one::<u16>(id).copied().unwrap_or(1));
    let query = discover::Query {
        text: (!words.is_empty()).then(|| words.join(" ")),
        task: leaf_matches
            .get_one::<String>("task")
            .cloned()
            .unwrap_or_default(),
        limit: number("limit"),
        candidates: number("candidates"),
        max_memory_percent: leaf_matches
            .get_one::<u64>("max-memory")
            .copied()
            .unwrap_or(50),
        min_score: f64::from(
            leaf_matches
                .get_one::<u8>("min-score")
                .copied()
                .unwrap_or(60),
        ),
        engine,
        npu,
        sort: leaf_matches
            .get_one::<Vec<discover::SortKey>>("sort")
            .cloned()
            .unwrap_or_default(),
        hf_token: std::env::var("HF_TOKEN").ok().filter(|t| !t.is_empty()),
    };
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    let world = discover::World {
        fetch: &discover::fetch,
        llmfit: &runtime::llmfit::fit_json,
        has_npu: discover::host_has_npu(),
        total_ram: system.total_memory(),
    };
    // Cleared when this function returns, on every path.
    let _spinner = progress::Indicator::spinner("searching Hugging Face");
    discover::discover(&query, &world, logger)
}

/// The engine `--backend <name>` designates: an engine name, or a
/// configured backend's identifier, whose engine is read from its runtime.
/// Only the second needs the configuration, and only then does a failed
/// load become this command's error.
fn discover_engine(
    name: &str,
    config: std::result::Result<&config::Config, &Error>,
) -> Result<discover::Engine> {
    if let Some(engine) = discover::Engine::parse(name) {
        return Ok(engine);
    }
    let config = config.map_err(|err| {
        Error::Config(format!(
            "--backend \"{name}\" is no engine ({}), and the configuration that could \
             name such a backend failed to load: {err}",
            discover::Engine::NAMES
        ))
    })?;
    let Some(backend) = config.backends.get(name) else {
        return Err(Error::Config(format!(
            "--backend \"{name}\" is neither an engine ({}) nor a configured backend \
             (available backends: {})",
            discover::Engine::NAMES,
            error::format_available(config.backends.keys())
        )));
    };
    discover::Engine::of_backend(backend).ok_or_else(|| {
        Error::Config(format!(
            "backend \"{name}\" starts no runtime npu recognizes as an engine: \
             pass --backend {}",
            discover::Engine::NAMES.replace(", ", "|")
        ))
    })
}

/// `npu model discover`'s arguments.
fn discover_command() -> clap::Command {
    clap::Command::new("discover")
        .about(
            "Search Hugging Face for models this host can run, judged by llmfit when it is \
             on PATH; --backend or --npu narrow the list",
        )
        .arg(
            clap::Arg::new("QUERY")
                .num_args(0..)
                .help("Words to search for (e.g. \"qwen coder\"); none lists the most downloaded"),
        )
        .arg(
            clap::Arg::new("task")
                .long("task")
                .value_name("TASK")
                .default_value("text-generation")
                .help("Hugging Face task the model must serve"),
        )
        .arg(
            clap::Arg::new("limit")
                .long("limit")
                .short('n')
                .value_name("N")
                .default_value("20")
                .value_parser(clap::value_parser!(u16).range(1..))
                .help("Most models listed"),
        )
        .arg(
            clap::Arg::new("candidates")
                .long("candidates")
                .value_name("N")
                .default_value("100")
                .value_parser(clap::value_parser!(u16).range(1..=1000))
                .help("Hugging Face results examined before filtering"),
        )
        .arg(
            clap::Arg::new("max-memory")
                .long("max-memory")
                .value_name("PERCENT")
                .default_value("50")
                .value_parser(clap::value_parser!(u64).range(1..=100))
                .help(
                    "Share of the total RAM one model's INT4 weights may take, in percent, \
                     for the models llmfit does not size",
                ),
        )
        .arg(
            clap::Arg::new("min-score")
                .long("min-score")
                .value_name("SCORE")
                .default_value("60")
                .value_parser(clap::value_parser!(u8).range(0..=100))
                .help("Lowest llmfit score kept, out of 100"),
        )
        .arg(
            clap::Arg::new("sort")
                .long("sort")
                .value_name("COLUMN[:asc|desc],...")
                .value_parser(|v: &str| discover::parse_sort(v))
                .help(
                    "Order by one or more columns (model, type, params, mem, license, downloads, \
                     score, fit, on); numbers default to desc, text to asc [default: downloads]",
                ),
        )
        .arg(
            clap::Arg::new("backend")
                .long("backend")
                .value_name("ENGINE|ID")
                .help(
                    "Only models packaged for this engine (openvino, llamacpp, mlx), or for \
                     the engine a configured backend runs",
                ),
        )
        .arg(
            clap::Arg::new("npu")
                .long("npu")
                .action(clap::ArgAction::SetTrue)
                .help("--backend openvino, on a host that has an Intel NPU"),
        )
}

/// `npu backend tune`'s arguments, kept out of [`add_builtins`] for size.
fn tune_command() -> clap::Command {
    clap::Command::new("tune")
        .about(
            "Size the context and memory of every NPU- and GPU-compiled model \
             from the model and the host's RAM, and write it",
        )
        .arg(
            clap::Arg::new("npu")
                .long("npu")
                .action(clap::ArgAction::SetTrue)
                .help("Tune the NPU models only [default: NPU and GPU, same limits]"),
        )
        .arg(
            clap::Arg::new("gpu")
                .long("gpu")
                .action(clap::ArgAction::SetTrue)
                .help("Tune the GPU models only"),
        )
        .arg(
            clap::Arg::new("max-models")
                .long("max-models")
                .value_name("N|all")
                .default_value("all")
                .value_parser(|v: &str| -> std::result::Result<usize, String> {
                    if v == "all" {
                        return Ok(0);
                    }
                    match v.parse::<usize>() {
                        Ok(n) if n >= 1 => Ok(n),
                        _ => Err("expected \"all\" or a number of models >= 1".into()),
                    }
                })
                .help("How many of the tuned models run at the same time"),
        )
        .arg(
            clap::Arg::new("max-memory")
                .long("max-memory")
                .value_name("PERCENT")
                .default_value("50")
                .value_parser(clap::value_parser!(u64).range(1..=100))
                .help("Share of the total RAM those models get together, in percent"),
        )
        .arg(
            clap::Arg::new("models-dir")
                .long("models-dir")
                .value_name("DIR")
                .value_parser(clap::value_parser!(std::path::PathBuf))
                .help("Directory holding the exports [default: $HOME/models]"),
        )
        .arg(
            clap::Arg::new("dry-run")
                .long("dry-run")
                .action(clap::ArgAction::SetTrue)
                .help("Print the plan without writing anything"),
        )
        .arg(
            clap::Arg::new("kv-u8")
                .long("kv-u8")
                .action(clap::ArgAction::SetTrue)
                .help("Store GPU KV caches as u8: about twice the context per GB"),
        )
}

/// Adds the CLI's built-ins — the `backend` group (`serve`, `stop`,
/// `status`, `logs`), the `config` group (`check`, `models`), `doctor`,
/// `describe`, `update` and the root `--version` flag — to the
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
    let model_arg = |help: &'static str| clap::Arg::new("MODEL").required(true).help(help);
    let backend = clap::Command::new("backend")
        .about("Manage the runtime of a model's backend: serve, stop, status, logs, tune")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(
            clap::Command::new("serve")
                .about("Start the runtime of the backend a model points at")
                .arg(model_arg(
                    "Identifier of the model to serve (e.g. \"qwen-fast\")",
                )),
        )
        .subcommand(
            clap::Command::new("stop")
                .about("Stop the runtime started for a model's backend")
                .arg(model_arg(
                    "Identifier of the model whose runtime is stopped",
                )),
        )
        .subcommand(
            clap::Command::new("status")
                .about("Report the state of every backend declaring a runtime"),
        )
        .subcommand(
            clap::Command::new("logs")
                .about("Stream the logs of the runtime started for a model's backend")
                .arg(model_arg("Identifier of the model whose runtime is read"))
                .arg(
                    clap::Arg::new("follow")
                        .long("follow")
                        .short('f')
                        .action(clap::ArgAction::SetTrue)
                        .help("Keep streaming as new lines arrive"),
                ),
        )
        .subcommand(tune_command());
    let config = clap::Command::new("config")
        .about("Inspect the configuration: check, models")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(clap::Command::new("check").about(DOCTOR_ABOUT))
        .subcommand(clap::Command::new("models").about("List configured models"));

    let model = clap::Command::new("model")
        .about("Find models for this host: discover")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(discover_command());

    cli.version(updater::VERSION)
        .subcommand(backend)
        .subcommand(config)
        .subcommand(model)
        .subcommand(clap::Command::new("doctor").about(DOCTOR_ABOUT))
        .subcommand(
            clap::Command::new("describe")
                .about("Describe a command, built-in or configured, as JSON")
                .arg(clap::Arg::new("COMMAND").required(true).num_args(1..).help(
                    "Command to describe, built-in or configured \
                             (e.g. \"doctor\", \"backend serve\" or \"git review\")",
                )),
        )
        .subcommand(
            clap::Command::new("update")
                .about("Download and install the latest npu release from GitHub"),
        )
        .subcommand(
            clap::Command::new("help")
                .about("Print this message or the help of the given command")
                .arg(
                    clap::Arg::new("COMMAND")
                        .num_args(0..)
                        .help("Command whose help is printed (e.g. \"backend serve\")"),
                ),
        )
}

/// `npu backend tune`: reads its flags and the host's RAM, then delegates to
/// [`tune::tune`], whose plan is the result.
fn backend_tune(
    config: &config::Config,
    leaf_matches: &clap::ArgMatches,
    env: &dyn Fn(&str) -> Option<String>,
    logger: log::Logger,
) -> Result<String> {
    let models_dir = match leaf_matches.get_one::<std::path::PathBuf>("models-dir") {
        Some(dir) => dir.clone(),
        None => std::path::PathBuf::from(
            env("HOME")
                .ok_or_else(|| Error::Config("HOME is not set: pass --models-dir".to_string()))?,
        )
        .join("models"),
    };
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    let (npu, gpu) = match (leaf_matches.get_flag("npu"), leaf_matches.get_flag("gpu")) {
        (false, false) => (true, true),
        flags => flags,
    };
    let limits = tune::Limits {
        npu,
        gpu,
        kv_u8: leaf_matches.get_flag("kv-u8"),
        max_models: leaf_matches
            .get_one::<usize>("max-models")
            .copied()
            .filter(|&n| n > 0),
        max_memory_percent: leaf_matches
            .get_one::<u64>("max-memory")
            .copied()
            .unwrap_or(50),
    };
    let report = tune::tune(
        config,
        &models_dir,
        system.total_memory(),
        limits,
        leaf_matches.get_flag("dry-run"),
        logger,
    )?;
    Ok(report)
}

/// Built-ins that run with a configuration that failed to load: the
/// dispatch in [`run`] handles them before it requires one.
const DEGRADED_MODE_BUILTINS: &[&[&str]] = &[
    &["doctor"],
    &["config", "check"],
    &["update"],
    &["describe"],
    &["model", "discover"],
];

/// The path `npu describe` was given, one segment per word, a word written
/// `git/review` counting as two.
fn describe_words(leaf_matches: &clap::ArgMatches) -> Vec<String> {
    leaf_matches
        .get_many::<String>("COMMAND")
        .into_iter()
        .flatten()
        .flat_map(|word| word.split('/'))
        .filter(|segment| !segment.is_empty())
        .map(str::to_string)
        .collect()
}

/// Describes `words` if they name a built-in; `None` lets the caller look
/// among the configured commands. A group (`backend`) is a built-in too.
fn describe_builtin(words: &[String]) -> Result<Option<String>> {
    let tree = add_builtins(clap::Command::new("npu"));
    let mut command = &tree;
    for word in words {
        match command.find_subcommand(word) {
            Some(sub) => command = sub,
            None => return Ok(None),
        }
    }
    if words.is_empty() {
        return Ok(None);
    }
    let path: Vec<&str> = words.iter().map(String::as_str).collect();
    builtin::describe_builtin(
        &path,
        command,
        DEGRADED_MODE_BUILTINS.contains(&path.as_slice()),
    )
    .map(Some)
}

/// Splits the root help into `Commands:` (the configured commands) and
/// `Built-ins:`, through a `help_template`.
///
/// `clap` has no per-subcommand heading: the built-ins are HIDDEN from its
/// `{subcommands}` list — hidden, not removed, they parse exactly as
/// before — and rendered by hand in the template, in `clap`'s own palette
/// (`style.rs`), which `clap` strips when stdout is not a terminal. The `help`
/// subcommand `clap` would generate is disabled: `npu help` is a built-in
/// of ours, listed with the others (cf. [`help`]).
fn sectioned_help(cli: clap::Command, load_failed: bool) -> clap::Command {
    let names: Vec<String> = add_builtins(clap::Command::new("npu"))
        .get_subcommands()
        .map(|sub| sub.get_name().to_string())
        .collect();
    let configured = cli
        .get_subcommands()
        .filter(|sub| !names.iter().any(|name| name == sub.get_name()))
        .count();
    let width = names.iter().map(String::len).max().unwrap_or(0);

    let heading = |text: &str| style::paint(style::HEADER.underline(), text);
    let commands = if configured > 0 {
        "{subcommands}".to_string()
    } else if load_failed {
        "  none: the configuration failed to load; run \"npu doctor\"".to_string()
    } else {
        "  none configured yet".to_string()
    };
    let mut builtins = String::new();
    let mut cli = cli.disable_help_subcommand(true);
    for name in &names {
        let about = cli
            .find_subcommand(name)
            .and_then(clap::Command::get_about)
            .map(ToString::to_string)
            .unwrap_or_default();
        let padded = format!("{name:width$}");
        builtins = format!(
            "{builtins}\n  {}  {about}",
            style::paint(style::LITERAL, &padded)
        );
        cli = cli.mut_subcommand(name, |sub| sub.hide(true));
    }
    cli.help_template(format!(
        "{{about-with-newline}}{{usage-heading}} {{usage}}\n\n{}\n{commands}\n\n{}{builtins}\n\n\
         {}\n{{options}}{{after-help}}",
        heading("Commands:"),
        heading("Built-ins:"),
        heading("Options:"),
    ))
}

/// `doctor` and `config check` are the same command under two names.
const DOCTOR_ABOUT: &str = "Check the runtime environment: configuration, backend reachability, \
                            declared output schemas";

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
/// supplied on the command line) is simply omitted from the map: there is
/// no default value. If the
/// prompt still references this argument via `{{ args.NAME }}`,
/// `prompt::render` fails with an `Error::Config` naming the argument
/// rather than substituting an unrequested empty
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
/// Where a streamed answer's tokens go: `(model that answers, token)`.
type TokenSink<'a> = &'a dyn Fn(&str, &str);

/// What [`chat_with_fallback`] asks of the model and, failing it, of its
/// fallback.
#[derive(Clone, Copy)]
struct Ask<'a> {
    prompt: &'a str,
    schema: Option<&'a serde_json::Value>,
    stream: Option<TokenSink<'a>>,
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

fn chat_with_fallback(
    config: &config::Config,
    model: &config::Model,
    backend: &config::Backend,
    ask: &Ask<'_>,
    logger: log::Logger,
) -> Result<(String, String)> {
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
    let spinner = progress::Indicator::spinner(&format!("waiting for model \"{}\"", model.id));
    // Whether a streamed token already reached the terminal: from then on
    // the answer is committed to this model, and a failure can no longer
    // be absorbed by the fallback.
    let emitted = std::cell::Cell::new(false);
    let call = |backend: &config::Backend, model: &config::Model| {
        let on_token = |token: &str| {
            if let Some(stream) = stream {
                spinner.clear();
                emitted.set(true);
                stream(&model.id, token);
            }
        };
        let on_token: Option<&dyn Fn(&str)> = stream.map(|_| &on_token as &dyn Fn(&str));
        runtime::resolve_base_url(backend, &runtime::docker::runner).and_then(|base_url| {
            let request = backend::Request {
                prompt,
                schema,
                on_token,
            };
            backend::chat(backend, model, &base_url, &request, logger)
        })
    };

    let primary = match call(backend, model) {
        Ok(output) => return Ok((output, model.id.clone())),
        Err(Error::Backend(message)) if !emitted.get() => message,
        Err(other) => return Err(other),
    };

    let Some(fallback_id) = model.fallback.as_deref() else {
        return Err(Error::Backend(primary));
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
            Error::Backend(second) => Error::Backend(format!(
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
/// Extracted from `run` as-is, for the degraded-mode wiring: where this
/// sequence is called from can change, never its content nor its internal
/// order.
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

    // Schemas are configuration, knowable without the input: read here,
    // before `input::resolve`, for the same reason as `preflight` (see
    // this function's doc). Only the invoked command's schemas are read.
    let output_schema = match (&spec.output.format, &spec.output.schema) {
        (output::Format::Json, Some(path)) => Some(output::read_schema(path, &spec.file)?),
        _ => None,
    };
    let schemas = spec
        .schemas
        .iter()
        .map(|(id, path)| {
            let document = output::read_schema(path, &spec.file)?;
            Ok((id.clone(), document.to_string()))
        })
        .collect::<Result<std::collections::BTreeMap<_, _>>>()?;

    prompt::preflight(&spec.prompt, &args, &env, &schemas)?;

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

    let prompt = prompt::render(&spec.prompt, &input_text, &args, &env, &schemas)?;
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
        && spec.output.format == output::Format::Text
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
    let output = output::finalize(&spec.output, &raw_output, &spec.file)?;
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
fn framed_answer(output: &str, answered_by: &str) -> String {
    format!("{}{output}\n\n", answer_header(answered_by))
}

/// The opening of [`framed_answer`], alone: what a streamed answer prints
/// before its first token.
fn answer_header(answered_by: &str) -> String {
    format!(
        "\n{} {}\n",
        style::paint(style::ANSWER_MARK, "●"),
        style::paint(style::ANSWER_HEADER, answered_by)
    )
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
/// **Degraded mode.** Loading (`config::load_scopes` THEN
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
/// - `--version` and `update` run without the configuration, just like `doctor`;
/// - any OTHER invocation (business command, `models`, `serve`, `stop`,
///   `status`, `logs`, `describe`)
///   propagates the kept load error via `loaded?`, exit code 2.
///
/// When loading SUCCEEDS, the pipeline behaves as normal,
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
    let level = log::level_from_args(std::env::args());
    let logger = log::Logger::new(level);
    progress::init(level);

    let roots = scope::roots();
    logger.info(&format!(
        "scopes: {}",
        error::format_available(roots.iter().map(|root| root.display().to_string()))
    ));

    let loaded: Result<(config::Config, Vec<command::CommandSpec>)> = config::load_scopes(&roots)
        .and_then(|config| command::discover_scopes(&roots).map(|specs| (config, specs)));

    if let (Err(err), false) = (&loaded, skips_config()) {
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
    let cli = sectioned_help(add_builtins(build_cli(specs_for_cli)), loaded.is_err());
    // Kept for `npu help`, which re-parses `<path> --help` through it.
    let help_cli = cli.clone();
    let matches = cli.get_matches();

    let (path, leaf_matches) = selected_path(&matches);
    let route: Vec<&str> = path.iter().map(String::as_str).collect();

    if matches!(route.as_slice(), ["doctor"] | ["config", "check"]) {
        return Ok(doctor(&loaded));
    }

    // These two built-ins depend only on the binary itself and GitHub
    // Releases. Like `doctor`, they remain available in degraded mode: a
    // malformed AI configuration is unrelated to reading or updating npu.
    if route == ["help"] {
        return help(help_cli, leaf_matches);
    }

    if route == ["update"] {
        return update(logger);
    }

    // Needs no configuration: it looks at the host and at Hugging Face.
    if route == ["model", "discover"] {
        let config = loaded.as_ref().map(|(config, _)| config);
        anstream::println!("{}", model_discover(leaf_matches, config, logger)?);
        return Ok(0);
    }

    // A built-in is described from the `clap` tree alone: like `doctor`,
    // this works whatever state the configuration is in.
    if route == ["describe"]
        && let Some(json) = describe_builtin(&describe_words(leaf_matches))?
    {
        println!("{json}");
        return Ok(0);
    }

    // Any OTHER branch (business command, `models`, the lifecycle commands,
    // `describe`) requires a
    // successfully loaded configuration: propagates the error KEPT above,
    // code 2, exactly as before this phase (point 1 of the shared
    // contract).
    let (config, specs) = loaded?;

    if route == ["config", "models"] {
        println!("{}", builtin::format_models(&config));
        return Ok(0);
    }

    // The outside world the lifecycle commands act through, injected in one
    // place exactly like `probe` and `runner`: the real environment, the
    // real state directory, the real process table, the real signals and
    // the real TCP probe. `builtin.rs` and `runtime::process`'s tests build
    // their own, which is why no test in this suite needs a server
    // installed.
    let env = |name: &str| std::env::var(name).ok();
    let host = runtime::process::Host {
        env: &env,
        state: runtime::state::StateEnv::from_env(),
        inspect: &runtime::process::inspect,
        signal: &runtime::process::signal,
        probe: &builtin::tcp_probe,
    };

    if route == ["backend", "serve"] {
        // `MODEL` is declared `.required(true)` by `add_builtins`: clap has
        // already rejected the invocation if it is absent.
        let model_id = leaf_matches
            .get_one::<String>("MODEL")
            .map(String::as_str)
            .unwrap_or_default();
        // The started instance's identifier IS this command's result:
        // stdout, like every other built-in's report — the container name
        // for Docker, the pid for a process. The runtime's own output goes
        // to stderr or to its log file (cf. `runtime::docker::runner` and
        // `runtime::process::serve`).
        println!(
            "{}",
            builtin::serve(&config, model_id, &env, &runtime::docker::runner, &host)?
        );
        return Ok(0);
    }

    if route == ["backend", "stop"] {
        let model_id = leaf_matches
            .get_one::<String>("MODEL")
            .map(String::as_str)
            .unwrap_or_default();
        logger.info(&format!("stopping the runtime of model \"{model_id}\""));
        println!(
            "{}",
            builtin::stop(&config, model_id, &runtime::docker::runner, &host)?
        );
        return Ok(0);
    }

    if route == ["backend", "status"] {
        // The report IS the result: stdout, like `doctor` and `models`.
        println!(
            "{}",
            builtin::status(&config, &runtime::docker::runner, &host)?
        );
        return Ok(0);
    }

    if route == ["backend", "logs"] {
        let model_id = leaf_matches
            .get_one::<String>("MODEL")
            .map(String::as_str)
            .unwrap_or_default();
        let follow = leaf_matches.get_flag("follow");
        // Nothing is printed here: the runtime's own output is the result.
        // `runtime::docker::streamer` lets a container's two streams through
        // untouched, and `runtime::process::stdout_sink` writes a spawned
        // server's log file byte for byte (cf. their docs).
        builtin::logs(
            &config,
            model_id,
            follow,
            &runtime::docker::streamer,
            &host,
            &runtime::process::stdout_sink,
        )?;
        return Ok(0);
    }

    if route == ["backend", "tune"] {
        println!("{}", backend_tune(&config, leaf_matches, &env, logger)?);
        return Ok(0);
    }

    if route == ["describe"] {
        let key = describe_words(leaf_matches).join("/");
        let spec = find_command(&specs, &key)?;
        println!("{}", builtin::describe(spec, &config)?);
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
            // `command::CommandSpec.output`: these `lib.rs` tests bear on the
            // construction of the `clap` tree, never on the output
            // contract — `OutputSpec::default()` (text format, no schema,
            // no limit) is neutral for them.
            output: output::OutputSpec::default(),
            schemas: std::collections::BTreeMap::new(),
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
    fn post_update_check_parses_against_the_real_tree() {
        let cli = sectioned_help(add_builtins(build_cli(&[])), false);
        assert!(
            cli.try_get_matches_from(
                std::iter::once("npu").chain(POST_UPDATE_CHECK.iter().copied())
            )
            .is_ok()
        );
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

    // -- declared CLI arguments --------------------------------------------

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
             no default value"
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

/// `update` and `--version` never read the configuration: warning about it
/// there would blame the user's files for an operation they cannot break.
fn skips_config() -> bool {
    matches!(
        first_word(std::env::args().skip(1)).as_deref(),
        Some("update" | "--version" | "-V")
    )
}

/// First argument that is neither `--verbose`/`-v` nor its value,
/// read from the RAW command line (cf. [`log::level_from_args`]).
fn first_word<I: IntoIterator<Item = String>>(args: I) -> Option<String> {
    let mut expecting_value = false;
    for arg in args {
        if expecting_value {
            expecting_value = false;
        } else if arg == "--verbose" || arg == "-v" {
            expecting_value = true;
        } else if !arg.starts_with("--verbose=") && !arg.starts_with("-v") {
            return Some(arg);
        }
    }
    None
}

#[cfg(test)]
mod first_word_tests {
    use super::first_word;

    fn first(args: &[&str]) -> Option<String> {
        first_word(args.iter().map(ToString::to_string))
    }

    #[test]
    fn skips_flags_and_the_verbose_value() {
        assert_eq!(
            first(&["--verbose", "info", "update"]).as_deref(),
            Some("update")
        );
        assert_eq!(
            first(&["-v", "error", "version"]).as_deref(),
            Some("version")
        );
        assert_eq!(
            first(&["--verbose=info", "update"]).as_deref(),
            Some("update")
        );
        assert_eq!(
            first(&["-vinfo", "--version"]).as_deref(),
            Some("--version")
        );
        assert_eq!(first(&["--verbose", "warn"]), None);
    }
}

/// Runs `doctor` (or `config check`), whatever state loading ended in,
/// prints its report and returns its exit code.
fn doctor(loaded: &Result<(config::Config, Vec<command::CommandSpec>)>) -> i32 {
    // `doctor` ALWAYS runs, whether loading succeeded or failed: this
    // is precisely its point in degraded mode (point 3 of the shared
    // contract). `probe` is the REAL probe (`builtin::tcp_probe`),
    // never a stub — `builtin.rs`'s tests inject their own directly on
    // `builtin::doctor`, this function here only wires in the real
    // probe.
    let (config_ref, specs_ref, load_error_ref) = match loaded {
        Ok((config, specs)) => (Some(config), Some(specs.as_slice()), None),
        Err(err) => (None, None, Some(err)),
    };
    let checks = builtin::doctor(
        config_ref,
        specs_ref,
        load_error_ref,
        &builtin::Probes {
            backend: &builtin::tcp_probe,
            container: &runtime::docker::probe,
            command: &runtime::process::command_probe,
            runner: &runtime::docker::runner,
        },
    );
    anstream::println!("{}", builtin::format_doctor(&checks));
    builtin::doctor_exit_code(&checks)
}

/// `npu help <path…>` is `npu <path…> --help`: `clap` renders it, and an
/// unknown path is `clap`'s own usage error, exit `2`, nothing on stdout.
fn help(cli: clap::Command, leaf_matches: &clap::ArgMatches) -> Result<i32> {
    let path = leaf_matches
        .get_many::<String>("COMMAND")
        .into_iter()
        .flatten();
    let args = std::iter::once("npu".to_string())
        .chain(path.cloned())
        .chain(std::iter::once("--help".to_string()));
    match cli.try_get_matches_from(args) {
        Err(err) => {
            err.print()?;
            Ok(err.exit_code())
        }
        Ok(_) => Ok(0),
    }
}

/// The invocation `update` hands the freshly installed binary to judge the
/// configuration: `config models` loads every scope without touching the
/// network, so its only failure is a configuration error.
const POST_UPDATE_CHECK: &[&str] = &["--verbose", "error", "config", "models"];

/// Runs `npu update`, then asks the NEW binary whether it accepts the
/// configuration and, if not, points the user at the changelog and the docs.
fn update(logger: log::Logger) -> Result<i32> {
    let outcome = updater::update()?;
    println!("{outcome}");
    if let updater::Outcome::Updated { current, .. } = &outcome {
        // The configuration is judged by the binary just installed, not by
        // this one: a key added in the new release must not look invalid,
        // and a key it dropped must not look valid. Only a configuration
        // error (`2`) is reported; the update itself has already succeeded.
        let rejected = std::env::current_exe()
            .ok()
            .and_then(|exe| runtime::exit_code_of(&exe, POST_UPDATE_CHECK))
            == Some(Error::Config(String::new()).exit_code());
        if rejected {
            logger.warn(&format!(
                "your configuration is not valid for npu {current}; see the changelog \
                 (https://github.com/fmatsos/npu/blob/main/CHANGELOG.md) and the documentation \
                 (https://github.com/fmatsos/npu/tree/main/docs), then run \"npu doctor\""
            ));
        }
    }
    Ok(0)
}

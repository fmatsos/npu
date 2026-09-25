//! Generic execution engine for local AI commands.
//!
//! All the logic lives here: the binary (`main.rs`) is only a shell.
//! This is what makes the pipeline testable from `tests/`, which can only
//! import a library target.

pub mod backend;
pub mod builtin;
mod cli;
pub mod command;
pub mod config;
#[cfg(feature = "hardware-tooling")]
pub mod discover;
mod dispatch;
pub mod error;
mod exec;
pub mod input;
pub mod log;
mod mcp;
pub mod output;
pub mod progress;
pub mod prompt;
pub mod runtime;
pub mod scope;
mod stats;
pub mod style;
#[cfg(feature = "hardware-tooling")]
pub mod tune;
pub mod updater;
#[cfg(feature = "hardware-tooling")]
pub mod vendor;

pub use error::{Error, Result};

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
/// built-ins ([`cli::builtins::add_builtins`]), and with the discovered
/// business commands ONLY if loading succeeded (otherwise
/// `cli::build_cli(&[])`). Consequences:
/// - `--help` ALWAYS works, even with a broken configuration;
/// - a line on STDERR reports the load failure and points to `npu doctor`
///   — emitted BEFORE `cli.get_matches()`, because `--help` exits via
///   `clap`'s internal `std::process::exit` without ever returning through
///   the rest of this function (cf. `tests/clap_error_stdout_purity.rs`);
/// - `npu doctor` ALWAYS runs (before any other branch) and reports the
///   kept load error as a failed check (cf. `builtin::doctor`);
/// - `--version` and `update` run without the configuration, just like `doctor`;
/// - any OTHER invocation (business command, `models`, `serve`, `stop`,
///   `status`, `logs`, `describe`)
///   propagates the kept load error via `loaded?`, exit code 2.
///
/// When loading SUCCEEDS, the pipeline behaves as normal,
/// invariant included (cf. [`exec::execute_business_command`]'s doc).
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
    // FIRST, before any log or print: `CompleteEnv::complete`'s own
    // warning is "stdout should not be written to before this has had a
    // chance to run" — `COMPLETE=bash npu ...` IS the completion request,
    // and it writes the completion script to stdout itself, exiting the
    // process, when the environment variable is set; a silent no-op
    // otherwise. No built-in, no reserved name: `CLAUDE.md` forbids adding
    // a top-level built-in, and this needs neither.
    complete_env().complete();

    let (logger, error_format, config_dir_override) = read_raw_args();

    let roots = scope::roots(config_dir_override.clone());
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
    let cli = cli::sectioned_help(
        cli::builtins::add_builtins(cli::build_cli(specs_for_cli)),
        loaded.is_err(),
    );
    // Kept for `npu help`, which re-parses `<path> --help` through it.
    let help_cli = cli.clone();
    let matches = match cli.try_get_matches() {
        Ok(matches) => matches,
        Err(err) => exit_on_clap_error(&err, error_format),
    };

    let (path, leaf_matches) = cli::selected_path(&matches);
    let route_path: Vec<&str> = path.iter().map(String::as_str).collect();
    let cfg_dir = config_dir_override.as_deref();
    let route = dispatch::route_for(&route_path);

    // These routes depend only on the binary itself, the host and GitHub
    // Releases — or, for `Describe`, only on the `clap` tree — so they run
    // whatever state loading ended in (degraded mode). Everything else
    // (`Business` included) is handled below, after `loaded?` has
    // propagated any load error, exit code 2.
    match route {
        dispatch::Route::Doctor => {
            let project_scope = scope::resolved_project_scope(config_dir_override.clone());
            return Ok(cli::builtins::doctor(
                &loaded,
                project_scope.as_deref(),
                leaf_matches.get_flag("json"),
            ));
        }
        dispatch::Route::Help => return cli::builtins::help(help_cli, leaf_matches, error_format),
        dispatch::Route::Update => return cli::builtins::update(logger, cfg_dir),
        dispatch::Route::McpServe => {
            return mcp::serve(mcp::Server::new(loaded, logger)?).map(|()| 0);
        }
        #[cfg(feature = "hardware-tooling")]
        dispatch::Route::ModelDiscover => {
            let config = loaded.as_ref().map(|(config, _)| config);
            anstream::println!(
                "{}",
                cli::builtins::model_discover(leaf_matches, config, logger)?
            );
            return Ok(0);
        }
        dispatch::Route::ConfigSchema => return Ok(print_config_schema(leaf_matches)),
        dispatch::Route::Describe => {
            if let Some(json) =
                cli::builtins::describe_builtin(&cli::builtins::describe_words(leaf_matches))?
            {
                println!("{json}");
                return Ok(0);
            }
        }
        // `ModelDiscover` can only be routed to here with the feature off,
        // since `add_builtins` then never declares `model discover` in the
        // `clap` tree: this arm never actually runs, it only keeps the
        // match exhaustive over every `Route` variant.
        #[cfg(not(feature = "hardware-tooling"))]
        dispatch::Route::ModelDiscover => {}
        dispatch::Route::ConfigModels
        | dispatch::Route::ConfigTest
        | dispatch::Route::BackendServe
        | dispatch::Route::BackendStop
        | dispatch::Route::BackendStatus
        | dispatch::Route::BackendLogs
        | dispatch::Route::BackendTune
        | dispatch::Route::Business => {}
    }

    let (config, specs) = loaded?;
    let env = |name: &str| std::env::var(name).ok();

    match route {
        dispatch::Route::ConfigTest => {
            run_config_test(&roots, &specs, &config, leaf_matches, &env, logger)
        }
        dispatch::Route::ConfigModels => {
            let report = if leaf_matches.get_flag("json") {
                builtin::format_models_json(&config)
            } else {
                builtin::format_models(&config)
            };
            println!("{report}");
            Ok(0)
        }
        dispatch::Route::BackendServe
        | dispatch::Route::BackendStop
        | dispatch::Route::BackendStatus
        | dispatch::Route::BackendLogs
        | dispatch::Route::BackendTune => {
            // The outside world the lifecycle commands act through,
            // injected in one place exactly like `probe` and `runner`: the
            // real environment, the real state directory, the real process
            // table, the real signals and the real TCP probe. `src/builtin/`
            // and `runtime::process`'s tests build their own, which is why
            // no test in this suite needs a server installed.
            let host = runtime::process::Host {
                env: &env,
                state: runtime::state::StateEnv::from_env(),
                inspect: &runtime::process::inspect,
                signal: &runtime::process::signal,
                probe: &builtin::tcp_probe,
            };
            match run_backend_lifecycle(&route_path, &config, leaf_matches, &env, &host, logger)? {
                Some(outcome) => Ok(outcome),
                None => unreachable!("route matched a backend lifecycle command"),
            }
        }
        dispatch::Route::Describe => describe_command(leaf_matches, &specs, &config),
        dispatch::Route::Business => {
            run_business_command(&path, &specs, &config, leaf_matches, &env, logger)?;
            Ok(0)
        }
        dispatch::Route::Doctor
        | dispatch::Route::ConfigSchema
        | dispatch::Route::McpServe
        | dispatch::Route::Help
        | dispatch::Route::Update
        | dispatch::Route::ModelDiscover => {
            unreachable!("these routes already returned above, whatever state loading ended in")
        }
    }
}

/// `config schema <KIND>`: needs no configuration, so it runs whatever
/// state loading ended in.
fn print_config_schema(matches: &clap::ArgMatches) -> i32 {
    let kind = matches.get_one::<String>("KIND").map_or("", String::as_str);
    match builtin::config_schema(kind) {
        Some(schema) => {
            print!("{schema}");
            0
        }
        None => unreachable!("clap only accepts a kind listed in SCHEMA_KINDS"),
    }
}

fn run_business_command(
    path: &[String],
    specs: &[command::CommandSpec],
    config: &config::Config,
    matches: &clap::ArgMatches,
    env: &dyn Fn(&str) -> Option<String>,
    logger: log::Logger,
) -> Result<()> {
    let spec = cli::find_command(specs, &path.join("/"))?;
    exec::execute_business_command(
        spec,
        config,
        matches,
        logger,
        env,
        &input::resolve,
        std::io::IsTerminal::is_terminal(&std::io::stdout()),
    )
}

fn run_config_test(
    roots: &[std::path::PathBuf],
    specs: &[command::CommandSpec],
    config: &config::Config,
    matches: &clap::ArgMatches,
    env: &dyn Fn(&str) -> Option<String>,
    logger: log::Logger,
) -> Result<i32> {
    let words: Vec<&str> = matches
        .get_many::<String>("COMMAND")
        .into_iter()
        .flatten()
        .map(String::as_str)
        .collect();
    let selected = (!words.is_empty()).then(|| words.join("/"));
    let options = builtin::TestOptions {
        selected: selected.as_deref(),
        model_override: matches.get_one::<String>("model").map(String::as_str),
        repeat: matches.get_one::<u16>("repeat").copied().unwrap_or(1),
        dry_run: matches.get_flag("dry-run"),
        json: matches.get_flag("json"),
    };
    let (report, code) = builtin::run_tests(roots, specs, config, options, env, logger)?;
    println!("{report}");
    Ok(code)
}

/// Handles the `backend serve|stop|status|logs|tune` group: `Some(code)` if
/// `route` named one of them, `None` otherwise. Extracted from `run` as-is,
/// only to keep the two `match`es above small — the behaviour and order are
/// unchanged.
fn run_backend_lifecycle(
    route: &[&str],
    config: &config::Config,
    leaf_matches: &clap::ArgMatches,
    env: &dyn Fn(&str) -> Option<String>,
    host: &runtime::process::Host<'_>,
    logger: log::Logger,
) -> Result<Option<i32>> {
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
            builtin::serve(config, model_id, env, &runtime::docker::runner, host)?
        );
        return Ok(Some(0));
    }

    if route == ["backend", "stop"] {
        let model_id = leaf_matches
            .get_one::<String>("MODEL")
            .map(String::as_str)
            .unwrap_or_default();
        logger.info(&format!("stopping the runtime of model \"{model_id}\""));
        println!(
            "{}",
            builtin::stop(config, model_id, &runtime::docker::runner, host)?
        );
        return Ok(Some(0));
    }

    if route == ["backend", "status"] {
        // The report IS the result: stdout, like `doctor` and `models`.
        if leaf_matches.get_flag("json") {
            let rows = builtin::status_rows(config, &runtime::docker::runner, host);
            println!(
                "{}",
                serde_json::to_string(&rows).unwrap_or_else(|_| "[]".to_string())
            );
        } else {
            println!(
                "{}",
                builtin::status(config, &runtime::docker::runner, host)?
            );
        }
        return Ok(Some(0));
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
            config,
            model_id,
            follow,
            &runtime::docker::streamer,
            host,
            &runtime::process::stdout_sink,
        )?;
        return Ok(Some(0));
    }

    #[cfg(feature = "hardware-tooling")]
    if route == ["backend", "tune"] {
        println!(
            "{}",
            cli::builtins::backend_tune(config, leaf_matches, env, logger)?
        );
        return Ok(Some(0));
    }

    Ok(None)
}

/// Builds the `CompleteEnv` `run` checks first, from the SAME `clap` tree
/// shape `run` itself builds a few lines later: the built-ins
/// (`cli::builtins::add_builtins`) plus the discovered business commands
/// when loading succeeds, the built-ins alone (degraded mode) otherwise —
/// this factory is `Fn() -> clap::Command`, so it repeats that same
/// loading independently rather than sharing `run`'s own `loaded`, which
/// does not exist yet at this point (cf. this function's doc: it must run
/// before anything else). `--config-dir`/`NPU_CONFIG_DIR` are read again
/// here for the same reason: the project scope must be known before the
/// commands it declares can be listed for completion.
/// The tree is built with `cli::builtins::add_builtins` directly, NEVER
/// through `sectioned_help`: `sectioned_help` hides the built-ins
/// (`mut_subcommand(name, |sub| sub.hide(true))`) so they render under their
/// own "Built-ins:" heading in `--help` rather than clap's default
/// "Commands:" section — a presentation concern with no bearing here. A
/// `clap_complete` engine reads that same `hide` flag to decide what to
/// offer, so completing through the hidden tree would silently drop every
/// built-in group (`backend`, `config`, ...) from `npu <TAB>`.
/// The command line being completed. A dynamic completion request is
/// `npu -- npu <words…>`: the first `--` is the completion transport, not
/// the argument terminator, so it is dropped here before the raw scanners
/// (which stop at `--`) read the words. Anything that is not a completion
/// request is returned as is.
fn completed_line(args: impl IntoIterator<Item = String>) -> Vec<String> {
    let args: Vec<String> = args.into_iter().collect();
    match args.iter().position(|arg| arg == "--") {
        Some(transport) => args[transport + 1..].to_vec(),
        None => args,
    }
}

fn complete_env() -> clap_complete::CompleteEnv<'static, impl Fn() -> clap::Command> {
    clap_complete::CompleteEnv::with_factory(|| {
        let config_dir_override = scope::config_dir_from_args(completed_line(std::env::args()))
            .or_else(scope::config_dir_env_var);
        let roots = scope::roots(config_dir_override);
        let specs = config::load_scopes(&roots)
            .and_then(|_| command::discover_scopes(&roots))
            .unwrap_or_default();
        // `disable_help_subcommand`, same as `sectioned_help`: `clap`'s own
        // generated `help` subcommand would otherwise collide with the
        // `help` built-in `add_builtins` declares (cf. this crate's
        // CLAUDE.md, "Built-ins and the container lifecycle").
        cli::builtins::add_builtins(cli::build_cli(&specs)).disable_help_subcommand(true)
    })
}

/// Reads everything `run` needs from the RAW command line, before `clap`
/// parses anything: the diagnostic level (`log::level_from_args`), the
/// error-envelope format (`error::error_format_from_args`) and the
/// `--config-dir`/`NPU_CONFIG_DIR` override (the CLI flag wins over the
/// environment variable when both are set) — the project scope is resolved
/// to LOAD the configuration, before the `clap` tree built FROM that
/// configuration even exists. `progress::init` also happens here: it must
/// run before the degraded-mode warning below, same reason as the logger.
fn read_raw_args() -> (log::Logger, error::ErrorFormat, Option<std::path::PathBuf>) {
    let level = log::level_from_args(std::env::args());
    let logger = log::Logger::new(level);
    progress::init(level);
    let error_format = error::error_format_from_args(std::env::args());
    let config_dir_override =
        scope::config_dir_from_args(std::env::args()).or_else(scope::config_dir_env_var);
    (logger, error_format, config_dir_override)
}

/// `npu describe [COMMAND…]`, once the configuration has LOADED (`run`'s
/// pre-`loaded?` branch already handled a built-in path in degraded mode):
/// with no argument, the index of every describable path (built-in and
/// business, cf. `cli::builtins::describe_index`) — never an error naming a
/// missing required argument, which `COMMAND` no longer is; with one,
/// the description of the business command it names.
fn describe_command(
    leaf_matches: &clap::ArgMatches,
    specs: &[command::CommandSpec],
    config: &config::Config,
) -> Result<i32> {
    let words = cli::builtins::describe_words(leaf_matches);
    if words.is_empty() {
        println!("{}", cli::builtins::describe_index(specs));
        return Ok(0);
    }
    let key = words.join("/");
    let spec = cli::find_command(specs, &key)?;
    println!("{}", builtin::describe(spec, config)?);
    Ok(0)
}

/// Terminates the process for a `clap` parse failure, exactly as
/// `clap::Error::exit` would, except a genuine USAGE error (anything other
/// than `--help`/`--version`) is rendered under `format` first —
/// `error::render_clap_usage_error` — so `--error-format json` can envelope
/// it. Only `--help`/`--version` keep `clap`'s own stdout rendering and
/// exit `0` whatever `format` is, since they are not errors (cf.
/// `error::ErrorFormat`'s doc).
/// `DisplayHelpOnMissingArgumentOrSubcommand` (e.g. `npu backend` with no
/// further word) is a USAGE failure, not a help request — it still exits
/// `2` (`err.exit_code()`, `clap`'s own contract) but goes through the
/// same envelope as any other usage error, or a calling agent would get
/// unenvelopped text for exactly the case it is most likely to hit first.
/// Never returns.
fn exit_on_clap_error(err: &clap::Error, format: error::ErrorFormat) -> ! {
    if matches!(
        err.kind(),
        clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
    ) {
        err.exit()
    }
    eprint!("{}", error::render_clap_usage_error(err, format));
    std::process::exit(err.exit_code());
}

/// `update` and `--version` never read the configuration: warning about it
/// there would blame the user's files for an operation they cannot break.
fn skips_config() -> bool {
    matches!(
        first_word(std::env::args().skip(1)).as_deref(),
        Some("update" | "--version" | "-V")
    )
}

/// First argument that is neither a global flag (`--verbose`/`-v`,
/// `--error-format`, `--config-dir`) nor its value, read from the RAW
/// command line (cf. [`log::level_from_args`]). A global flag declared
/// after the actual first word (e.g. `npu classify --verbose info`) is
/// none of this function's concern: it only has to look PAST the flags
/// that can precede the first word, same idiom as `level_from_args`,
/// `error::error_format_from_args` and `scope::config_dir_from_args`.
fn first_word<I: IntoIterator<Item = String>>(args: I) -> Option<String> {
    let mut expecting_value = false;
    for arg in args {
        if expecting_value {
            expecting_value = false;
        } else if arg == "--verbose"
            || arg == "-v"
            || arg == "--error-format"
            || arg == "--config-dir"
        {
            expecting_value = true;
        } else if !arg.starts_with("--verbose=")
            && !arg.starts_with("-v")
            && !arg.starts_with("--error-format=")
            && !arg.starts_with("--config-dir=")
        {
            return Some(arg);
        }
    }
    None
}

#[cfg(test)]
mod first_word_tests {
    use super::first_word;

    #[test]
    fn a_completion_request_keeps_the_config_dir_of_the_completed_line() {
        let argv = ["npu", "--", "npu", "--config-dir", "/custom/.npu", "cl"].map(String::from);
        assert_eq!(
            crate::scope::config_dir_from_args(super::completed_line(argv)),
            Some(std::path::PathBuf::from("/custom/.npu"))
        );
    }

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

    /// `npu --error-format json --version` with a broken configuration must
    /// print no degraded-mode warning: `skips_config` reads `--version` as
    /// the first word, not `--error-format` or its value `json`.
    #[test]
    fn skips_error_format_and_its_value() {
        assert_eq!(
            first(&["--error-format", "json", "--version"]).as_deref(),
            Some("--version")
        );
        assert_eq!(
            first(&["--error-format=json", "update"]).as_deref(),
            Some("update")
        );
    }

    #[test]
    fn skips_config_dir_and_its_value() {
        assert_eq!(
            first(&["--config-dir", "/x", "update"]).as_deref(),
            Some("update")
        );
        assert_eq!(
            first(&["--config-dir=/x", "--version"]).as_deref(),
            Some("--version")
        );
    }
}

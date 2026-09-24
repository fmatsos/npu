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
pub mod output;
pub mod progress;
pub mod prompt;
pub mod runtime;
pub mod scope;
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
    let cli = cli::sectioned_help(
        cli::builtins::add_builtins(cli::build_cli(specs_for_cli)),
        loaded.is_err(),
    );
    // Kept for `npu help`, which re-parses `<path> --help` through it.
    let help_cli = cli.clone();
    let matches = match cli.try_get_matches() {
        Ok(matches) => matches,
        Err(err) => err.exit(),
    };

    let (path, leaf_matches) = cli::selected_path(&matches);
    let route_path: Vec<&str> = path.iter().map(String::as_str).collect();
    let route = dispatch::route_for(&route_path);

    // These routes depend only on the binary itself, the host and GitHub
    // Releases — or, for `Describe`, only on the `clap` tree — so they run
    // whatever state loading ended in (degraded mode). Everything else
    // (`Business` included) is handled below, after `loaded?` has
    // propagated any load error, exit code 2.
    match route {
        dispatch::Route::Doctor => return Ok(cli::builtins::doctor(&loaded)),
        dispatch::Route::Help => return cli::builtins::help(help_cli, leaf_matches),
        dispatch::Route::Update => return cli::builtins::update(logger),
        #[cfg(feature = "hardware-tooling")]
        dispatch::Route::ModelDiscover => {
            let config = loaded.as_ref().map(|(config, _)| config);
            anstream::println!(
                "{}",
                cli::builtins::model_discover(leaf_matches, config, logger)?
            );
            return Ok(0);
        }
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
        dispatch::Route::ConfigModels => {
            println!("{}", builtin::format_models(&config));
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
        dispatch::Route::Describe => {
            let key = cli::builtins::describe_words(leaf_matches).join("/");
            let spec = cli::find_command(&specs, &key)?;
            println!("{}", builtin::describe(spec, &config)?);
            Ok(0)
        }
        dispatch::Route::Business => {
            let key = path.join("/");
            let spec = cli::find_command(&specs, &key)?;
            exec::execute_business_command(
                spec,
                &config,
                leaf_matches,
                logger,
                &env,
                &input::resolve,
                std::io::IsTerminal::is_terminal(&std::io::stdout()),
            )?;
            Ok(0)
        }
        dispatch::Route::Doctor
        | dispatch::Route::Help
        | dispatch::Route::Update
        | dispatch::Route::ModelDiscover => {
            unreachable!("these routes already returned above, whatever state loading ended in")
        }
    }
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
        println!(
            "{}",
            builtin::status(config, &runtime::docker::runner, host)?
        );
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

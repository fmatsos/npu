//! The built-ins added on top of the tree built by [`super::build_cli`]: the
//! `backend` group (`serve`, `stop`, `status`, `logs`, `tune`), the `config`
//! group (`check`, `models`), the `model` group (`discover`), `doctor`,
//! `describe`, `update` and `help`.

/// `npu model discover`: reads its flags, the host's RAM and NPU, then
/// delegates to [`crate::discover::discover`], whose report is the result.
pub(crate) fn model_discover(
    leaf_matches: &clap::ArgMatches,
    config: std::result::Result<&crate::config::Config, &crate::Error>,
    logger: crate::log::Logger,
) -> crate::Result<String> {
    let npu = leaf_matches.get_flag("npu");
    let engine = match leaf_matches.get_one::<String>("backend") {
        None => npu.then_some(crate::discover::Engine::OpenVino),
        Some(name) => Some(discover_engine(name, config)?),
    };
    if npu && engine != Some(crate::discover::Engine::OpenVino) {
        return Err(crate::Error::Config(
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
    let query = crate::discover::Query {
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
            .get_one::<Vec<crate::discover::SortKey>>("sort")
            .cloned()
            .unwrap_or_default(),
        hf_token: std::env::var("HF_TOKEN").ok().filter(|t| !t.is_empty()),
    };
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    let world = crate::discover::World {
        fetch: &crate::discover::fetch,
        llmfit: &crate::runtime::llmfit::fit_json,
        has_npu: crate::discover::host_has_npu(),
        total_ram: system.total_memory(),
    };
    // Cleared when this function returns, on every path.
    let _spinner = crate::progress::Indicator::spinner("searching Hugging Face");
    crate::discover::discover(&query, &world, logger)
}

/// The engine `--backend <name>` designates: an engine name, or a
/// configured backend's identifier, whose engine is read from its runtime.
/// Only the second needs the configuration, and only then does a failed
/// load become this command's error.
fn discover_engine(
    name: &str,
    config: std::result::Result<&crate::config::Config, &crate::Error>,
) -> crate::Result<crate::discover::Engine> {
    if let Some(engine) = crate::discover::Engine::parse(name) {
        return Ok(engine);
    }
    let config = config.map_err(|err| {
        crate::Error::Config(format!(
            "--backend \"{name}\" is no engine ({}), and the configuration that could \
             name such a backend failed to load: {err}",
            crate::discover::Engine::NAMES
        ))
    })?;
    let Some(backend) = config.backends.get(name) else {
        return Err(crate::Error::Config(format!(
            "--backend \"{name}\" is neither an engine ({}) nor a configured backend \
             (available backends: {})",
            crate::discover::Engine::NAMES,
            crate::error::format_available(config.backends.keys())
        )));
    };
    crate::discover::Engine::of_backend(backend).ok_or_else(|| {
        crate::Error::Config(format!(
            "backend \"{name}\" starts no runtime npu recognizes as an engine: \
             pass --backend {}",
            crate::discover::Engine::NAMES.replace(", ", "|")
        ))
    })
}

/// `npu model discover`'s arguments.
pub(crate) fn discover_command() -> clap::Command {
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
                .value_parser(|v: &str| crate::discover::parse_sort(v))
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
pub(crate) fn tune_command() -> clap::Command {
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
/// Called UNCONDITIONALLY by `crate::run`, including when loading the
/// configuration has failed (degraded mode): a broken configuration must
/// NEVER deprive `--help` of `doctor`, since `doctor` is precisely the tool
/// meant to diagnose its cause. `command::reject_reserved_path` already
/// guarantees, upstream at discovery time, that no business command can
/// carry one of these names as its first path segment: these
/// `subcommand()` calls can therefore never collide with the ones added by
/// [`super::build_cli`].
pub(crate) fn add_builtins(cli: clap::Command) -> clap::Command {
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

    cli.version(crate::updater::VERSION)
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
/// [`crate::tune::tune`], whose plan is the result.
pub(crate) fn backend_tune(
    config: &crate::config::Config,
    leaf_matches: &clap::ArgMatches,
    env: &dyn Fn(&str) -> Option<String>,
    logger: crate::log::Logger,
) -> crate::Result<String> {
    let models_dir = match leaf_matches.get_one::<std::path::PathBuf>("models-dir") {
        Some(dir) => dir.clone(),
        None => std::path::PathBuf::from(env("HOME").ok_or_else(|| {
            crate::Error::Config("HOME is not set: pass --models-dir".to_string())
        })?)
        .join("models"),
    };
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    let (npu, gpu) = match (leaf_matches.get_flag("npu"), leaf_matches.get_flag("gpu")) {
        (false, false) => (true, true),
        flags => flags,
    };
    let limits = crate::tune::Limits {
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
    let report = crate::tune::tune(
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
/// dispatch in `crate::run` handles them before it requires one.
pub(crate) const DEGRADED_MODE_BUILTINS: &[&[&str]] = &[
    &["doctor"],
    &["config", "check"],
    &["update"],
    &["describe"],
    &["model", "discover"],
];

/// The path `npu describe` was given, one segment per word, a word written
/// `git/review` counting as two.
pub(crate) fn describe_words(leaf_matches: &clap::ArgMatches) -> Vec<String> {
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
pub(crate) fn describe_builtin(words: &[String]) -> crate::Result<Option<String>> {
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
    crate::builtin::describe_builtin(
        &path,
        command,
        DEGRADED_MODE_BUILTINS.contains(&path.as_slice()),
    )
    .map(Some)
}

/// `doctor` and `config check` are the same command under two names.
pub(crate) const DOCTOR_ABOUT: &str = "Check the runtime environment: configuration, backend reachability, \
                            declared output schemas";

/// Runs `doctor` (or `config check`), whatever state loading ended in,
/// prints its report and returns its exit code.
pub(crate) fn doctor(
    loaded: &crate::Result<(crate::config::Config, Vec<crate::command::CommandSpec>)>,
) -> i32 {
    // `doctor` ALWAYS runs, whether loading succeeded or failed: this
    // is precisely its point in degraded mode. `probe` is the REAL probe
    // (`builtin::tcp_probe`), never a stub — `builtin.rs`'s tests inject
    // their own directly on `builtin::doctor`, this function here only
    // wires in the real probe.
    let (config_ref, specs_ref, load_error_ref) = match loaded {
        Ok((config, specs)) => (Some(config), Some(specs.as_slice()), None),
        Err(err) => (None, None, Some(err)),
    };
    let checks = crate::builtin::doctor(
        config_ref,
        specs_ref,
        load_error_ref,
        &crate::builtin::Probes {
            backend: &crate::builtin::tcp_probe,
            container: &crate::runtime::docker::probe,
            command: &crate::runtime::process::command_probe,
            runner: &crate::runtime::docker::runner,
        },
    );
    anstream::println!("{}", crate::builtin::format_doctor(&checks));
    crate::builtin::doctor_exit_code(&checks)
}

/// `npu help <path…>` is `npu <path…> --help`: `clap` renders it, and an
/// unknown path is `clap`'s own usage error, exit `2`, nothing on stdout.
pub(crate) fn help(cli: clap::Command, leaf_matches: &clap::ArgMatches) -> crate::Result<i32> {
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
pub(crate) const POST_UPDATE_CHECK: &[&str] = &["--verbose", "error", "config", "models"];

/// Runs `npu update`, then asks the NEW binary whether it accepts the
/// configuration and, if not, points the user at the changelog and the docs.
pub(crate) fn update(logger: crate::log::Logger) -> crate::Result<i32> {
    let outcome = crate::updater::update()?;
    println!("{outcome}");
    if let crate::updater::Outcome::Updated { current, .. } = &outcome {
        // The configuration is judged by the binary just installed, not by
        // this one: a key added in the new release must not look invalid,
        // and a key it dropped must not look valid. Only a configuration
        // error (`2`) is reported; the update itself has already succeeded.
        let rejected = std::env::current_exe()
            .ok()
            .and_then(|exe| crate::runtime::exit_code_of(&exe, POST_UPDATE_CHECK))
            == Some(crate::Error::Config(String::new()).exit_code());
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

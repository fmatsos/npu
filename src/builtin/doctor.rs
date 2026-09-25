//! `npu doctor` (also `config check`): reads the current state (loaded
//! configuration, discovered commands, and whether loading itself failed),
//! runs every check, and hands back a report ([`Check`]) that the caller
//! formats and turns into an exit code. Never touches the console itself
//! (cf. `super`'s module doc): every reachability probe is injected through
//! [`Probes`], which is why no test in this module needs a network, Docker
//! or an inference server.

use super::{Check, CheckKind, Status};

pub struct Probes<'a> {
    /// Is a backend's `base_url` accepting connections? The real one is
    /// [`super::net::tcp_probe`].
    pub backend: &'a dyn Fn(&str) -> Result<(), String>,
    /// Is the container runtime usable? The real one is
    /// [`crate::runtime::docker::probe`].
    pub container: &'a dyn Fn() -> Result<(), String>,
    /// Is a process runtime's command runnable? The real one is
    /// [`crate::runtime::process::command_probe`].
    pub command: &'a dyn Fn(&str) -> Result<(), String>,
    /// Runs the container runtime, for the `port = "auto"` backends whose
    /// `base_url` can only be completed by asking it.
    pub runner: &'a dyn Fn(&[String]) -> crate::Result<String>,
}

// `missing_debug_implementations` is a warning, and warnings are errors: a
// struct of `&dyn Fn` cannot derive `Debug`, and the closures have nothing
// to print.
impl std::fmt::Debug for Probes<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Probes").finish_non_exhaustive()
    }
}

/// Check: the project scope actually used to load the configuration — an
/// `Ok` line naming the directory when
/// [`crate::scope::resolved_project_scope`] found one (an override or a
/// `.npu` found by walking up from `cwd`), absent entirely otherwise (no
/// line rather than a line claiming "none": a project with no local scope
/// at all is not a failure, cf. this module's "deliberate omission" doc).
fn check_project_scope(project_scope: Option<&std::path::Path>) -> Option<Check> {
    let scope = project_scope?;
    Some(Check {
        kind: CheckKind::Config,
        label: format!("project scope ({})", scope.display()),
        status: Status::Ok,
    })
}

/// Check (a): was the configuration loaded successfully?
///
/// `load_error` carries the error KEPT by the degraded mode of `lib.rs::run`
/// (shared-contract decision 1) rather than propagated immediately: this is
/// precisely what lets `doctor` run despite a broken configuration.
/// The failure message is that of `crate::Error` itself (already
/// prefixed with `configuration error: `, cf. `error.rs`), never rebuilt
/// by hand.
fn check_config_loaded(load_error: Option<&crate::Error>) -> Check {
    let status = match load_error {
        Some(err) => Status::Failed(err.to_string()),
        None => Status::Ok,
    };
    Check {
        kind: CheckKind::Config,
        label: "configuration loaded".to_string(),
        status,
    }
}

/// Check (b): is each configured backend reachable? Iterates over
/// `config.backends` sorted by identifier (`BTreeMap`/`HashMap` is not
/// ordered otherwise), so that the report order is deterministic regardless
/// of the underlying `HashMap`'s iteration order.
///
/// `probe` is injected (never [`super::net::tcp_probe`] called directly): this is what
/// makes this function, and therefore [`doctor`] as a whole, testable
/// without ever opening a single socket.
fn check_backends_reachable(
    config: &crate::config::Config,
    probe: &dyn Fn(&str) -> Result<(), String>,
    runner: &dyn Fn(&[String]) -> crate::Result<String>,
) -> Vec<Check> {
    let mut ids: Vec<&String> = config.backends.keys().collect();
    ids.sort_unstable();

    ids.into_iter()
        .map(|id| {
            // `id` comes from `config.backends`'s keys: the entry necessarily exists.
            let backend = &config.backends[id];
            // A `port = "auto"` URL is only complete once its container
            // runs, so failing to resolve it is a REACHABILITY failure like
            // the probe's own — exit 3, never 2: nothing in the files is
            // wrong, the runtime is simply not up.
            let status = match crate::runtime::resolve_base_url(backend, runner)
                .map_err(|err| err.to_string())
                .and_then(|base_url| probe(&base_url))
            {
                Ok(()) => Status::Ok,
                Err(message) => Status::Failed(message),
            };
            Check {
                kind: CheckKind::Reachability,
                label: format!("backend \"{id}\" reachable"),
                status,
            }
        })
        .collect()
}

/// Check (f): is the container runtime usable, when at least one backend
/// declares a Docker runtime?
///
/// Produces NO check at all when no backend declares one: Docker is an
/// OPTIONAL prerequisite of this CLI, and a failed check on a machine that
/// never asked for a container would turn a healthy configuration into a
/// non-zero exit code.
///
/// `CheckKind::Reachability`, like the backend probe and for the same
/// reason: a missing or stopped container runtime is something to START, not
/// a file to fix — it must never produce the code that means "your files are
/// wrong".
fn check_container_runtime(
    config: &crate::config::Config,
    container_probe: &dyn Fn() -> Result<(), String>,
) -> Vec<Check> {
    // `config::docker_of` and not a `matches!` of its own: that helper is
    // the crate's single family test, written as an exhaustive `match`, so a
    // new runtime family makes this site fail to compile rather than
    // silently answer `false` for it.
    if !config
        .backends
        .values()
        .any(|backend| crate::config::docker_of(backend).is_some())
    {
        return Vec::new();
    }

    let status = match container_probe() {
        Ok(()) => Status::Ok,
        Err(message) => Status::Failed(message),
    };

    vec![Check {
        kind: CheckKind::Reachability,
        label: "container runtime available".to_string(),
        status,
    }]
}

/// Check (g): is each command a process runtime declares actually runnable?
///
/// Modelled on [`check_container_runtime`], with the same two properties:
/// no check AT ALL when no backend declares a process runtime (a machine
/// that never asked for one must not be penalized), and
/// `CheckKind::Reachability` so that a missing command gives exit `3` and
/// never `2` — an absent executable is something to install, not a file to
/// fix.
///
/// One check per DISTINCT command rather than one for the family: unlike
/// Docker, whose binary is fixed, what a process runtime needs is whatever
/// its author wrote, and a report that did not name it would send its reader
/// looking through every backend file for the missing one. Commands are
/// deduplicated and sorted, so the report stays deterministic and a command
/// shared by three backends is probed once.
fn check_runtime_commands(
    config: &crate::config::Config,
    command_probe: &dyn Fn(&str) -> Result<(), String>,
) -> Vec<Check> {
    // `config::process_of` and not a `matches!` of its own, for the same
    // reason as above: the family test is exhaustive, so a new family makes
    // this site fail to compile instead of silently skipping it.
    let mut commands: Vec<&str> = config
        .backends
        .values()
        .filter_map(|backend| {
            crate::config::process_of(backend).map(|process| process.command.as_str())
        })
        // A `command` carrying a placeholder is only known once `npu serve`
        // renders it against a model, and `doctor` has no model: reporting
        // the template itself as a missing executable would turn a valid
        // configuration red and tell its operator to install a binary named
        // `{{ env.LLAMA_BIN }}`. A check that cannot be performed is not
        // emitted; nothing is silently passed, since `serve` still resolves
        // it and fails naming the rendered value.
        .filter(|command| crate::prompt::placeholders(command).is_ok_and(|found| found.is_empty()))
        .collect();
    commands.sort_unstable();
    commands.dedup();

    commands
        .into_iter()
        .map(|command| Check {
            kind: CheckKind::Reachability,
            label: format!("runtime command \"{command}\" available"),
            status: match command_probe(command) {
                Ok(()) => Status::Ok,
                Err(message) => Status::Failed(message),
            },
        })
        .collect()
}

/// Check (c): for each configured model, does its backend exist,
/// and does it expose the operation that model declares? A single [`Check`]
/// per model, covering both — a model whose backend is absent cannot
/// expose an operation to check anyway. Sorted by model
/// identifier, same determinism reason as [`check_backends_reachable`].
fn check_models(config: &crate::config::Config) -> Vec<Check> {
    let mut ids: Vec<&String> = config.models.keys().collect();
    ids.sort_unstable();

    ids.into_iter()
        .map(|id| {
            // `id` comes from `config.models`'s keys: the entry necessarily exists.
            let model = &config.models[id];
            let status = match config.backends.get(&model.backend) {
                None => Status::Failed(format!(
                    "backend \"{}\" not found (referenced by model \"{id}\"; available \
                     backends: {})",
                    model.backend,
                    crate::error::format_available(config.backends.keys())
                )),
                Some(backend) => {
                    if backend.operations.contains_key(&model.operation) {
                        Status::Ok
                    } else {
                        Status::Failed(format!(
                            "backend \"{}\" does not expose operation \"{}\" declared by model \
                             \"{id}\" (available operations: {})",
                            backend.id,
                            model.operation,
                            crate::error::format_available(backend.operations.keys())
                        ))
                    }
                }
            };
            Check {
                kind: CheckKind::Config,
                label: format!("model \"{id}\""),
                status,
            }
        })
        .collect()
}

/// Sorts `commands` by full path (`path.join("/")`), so that the order of
/// the [`doctor`] report is deterministic regardless of the order in which
/// files are discovered on disk — same reason as the sort in
/// `command::discover_scopes` itself.
fn sorted_commands(commands: &[crate::command::CommandSpec]) -> Vec<&crate::command::CommandSpec> {
    let mut sorted: Vec<&crate::command::CommandSpec> = commands.iter().collect();
    sorted.sort_by_key(|spec| spec.path.join("/"));
    sorted
}

/// Check (d): for each discovered command, in ALL scopes
/// (not just the one possibly invoked), does its model exist?
fn check_commands_model(
    config: &crate::config::Config,
    commands: &[crate::command::CommandSpec],
) -> Vec<Check> {
    sorted_commands(commands)
        .into_iter()
        .map(|spec| {
            let path = spec.path.join("/");
            let status = if config.models.contains_key(&spec.model) {
                Status::Ok
            } else {
                Status::Failed(format!(
                    "model \"{}\" not found (referenced by command \"{path}\"; available \
                     models: {})",
                    spec.model,
                    crate::error::format_available(config.models.keys())
                ))
            };
            Check {
                kind: CheckKind::Config,
                label: format!("command \"{path}\": model"),
                status,
            }
        })
        .collect()
}

/// Check (e): for each command declaring an output schema
/// (`[output].schema`), in ALL scopes, is that schema present,
/// readable, valid JSON, and a compilable JSON Schema? This is the
/// EXHAUSTIVE work deliberately deferred to `doctor` (cf.
/// `output.rs`, `command::resolve_schema_path`: a schema's existence and
/// compilation are only checked, outside `doctor`, at the moment the
/// command that requires it is actually invoked). Reuses
/// `output::compile_schema` — `pub(crate)` for this purpose — rather than
/// writing a second implementation of schema compilation: the three
/// distinct error messages (absent/unreadable, invalid JSON, schema
/// syntactically invalid) it already produces are exactly the ones
/// this check must report.
///
/// A command without `[output].schema` (text format, or JSON without a
/// schema — both explicitly allowed by `command::convert_output`)
/// produces no output-schema [`Check`]: there is nothing to check. Each
/// entry of its `[schemas]` table gets one check of its own, compiled the
/// same way: a schema pasted into a prompt must be one too. Each
/// `[partials]` entry gets one as well: present, UTF-8, placeholder-free.
fn check_commands_output_schema(commands: &[crate::command::CommandSpec]) -> Vec<Check> {
    let check = |label: String, schema_path: &std::path::Path, file: &std::path::Path| Check {
        kind: CheckKind::Config,
        label,
        status: match crate::output::compile_schema(schema_path, file) {
            Ok(_validator) => Status::Ok,
            Err(err) => Status::Failed(err.to_string()),
        },
    };
    sorted_commands(commands)
        .into_iter()
        .flat_map(|spec| {
            let path = spec.path.join("/");
            let output = spec.output.schema.as_deref().map(|schema_path| {
                check(
                    format!("command \"{path}\": output schema"),
                    schema_path,
                    &spec.file,
                )
            });
            let declared = spec.schemas.iter().map(move |(id, schema_path)| {
                check(
                    format!("command \"{}\": schema \"{id}\"", spec.path.join("/")),
                    schema_path,
                    &spec.file,
                )
            });
            let partials = spec.partials.iter().map(move |(id, partial_path)| Check {
                kind: CheckKind::Config,
                label: format!("command \"{}\": partial \"{id}\"", spec.path.join("/")),
                status: match crate::prompt::read_partial(partial_path, &spec.file) {
                    Ok(_text) => Status::Ok,
                    Err(err) => Status::Failed(err.to_string()),
                },
            });
            output
                .into_iter()
                .chain(declared)
                .chain(partials)
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Runs all `npu doctor` checks and
/// returns the report — without ever writing to the console or touching
/// the network (`probe` is injected); only check (e) touches disk, by
/// reading the declared schema files.
///
/// `config`/`commands` and `load_error` reflect the degraded mode of
/// `lib.rs::run`: when loading fails, `run` KEEPS the error
/// instead of propagating it, so that `doctor` can still run and
/// report it as a failed check (a). This function does not assume that
/// `config`/`commands`/`load_error` vary as a block, though: each
/// family of checks (b/c, then d/e) only runs if the data it needs is
/// actually available, which stays correct whether the caller treats
/// loading as a single atomic operation (the case expected in
/// practice) or distinguishes a genuine configuration
/// failure from a command discovery failure.
#[must_use]
pub fn doctor(
    config: Option<&crate::config::Config>,
    commands: Option<&[crate::command::CommandSpec]>,
    load_error: Option<&crate::Error>,
    project_scope: Option<&std::path::Path>,
    probes: &Probes<'_>,
) -> Vec<Check> {
    let mut checks = vec![check_config_loaded(load_error)];
    checks.extend(check_project_scope(project_scope));

    if let Some(config) = config {
        checks.extend(check_backends_reachable(
            config,
            probes.backend,
            probes.runner,
        ));
        checks.extend(check_container_runtime(config, probes.container));
        checks.extend(check_runtime_commands(config, probes.command));
        checks.extend(check_models(config));
    }

    if let (Some(config), Some(commands)) = (config, commands) {
        checks.extend(check_commands_model(config, commands));
        checks.extend(check_commands_output_schema(commands));
    }

    checks
}

/// Formats the `doctor` report for stdout:
/// a checkmark (`✓`) followed by the label for each successful check, a
/// cross (`✗`) followed by the label THEN the message for each failure —
/// never the reverse, or the message explaining the failure would end up
/// without context on what it concerns.
#[must_use]
pub fn format_doctor(checks: &[Check]) -> String {
    checks
        .iter()
        .map(|check| match &check.status {
            Status::Ok => format!(
                "{} {}",
                crate::style::paint(crate::style::OK, "✓"),
                check.label
            ),
            Status::Failed(message) => format!(
                "{} {}: {message}",
                crate::style::paint(crate::style::ERROR, "✗"),
                check.label
            ),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Exit code of the `doctor` report:
/// - `0` if all checks pass;
/// - `2` if at least one CONFIGURATION check (a, c, d, e) fails —
///   configuration takes priority, including when a reachability check
///   (b) fails at the same time;
/// - `3` if ONLY reachability (b) fails.
///
/// Distinguishes the two failure families via [`Check::kind`], never via
/// the text of [`Check::label`]: a label is display — it can be
/// reworded, translated, or given a new suffix without notice — while
/// the category is a machine contract that this exit code
/// must keep honoring no matter what happens to the label.
#[must_use]
pub fn doctor_exit_code(checks: &[Check]) -> i32 {
    let mut config_failed = false;
    let mut reachability_failed = false;

    for check in checks {
        let Status::Failed(_) = &check.status else {
            continue;
        };
        match check.kind {
            CheckKind::Reachability => reachability_failed = true,
            CheckKind::Config => config_failed = true,
        }
    }

    if config_failed {
        2
    } else if reachability_failed {
        3
    } else {
        0
    }
}

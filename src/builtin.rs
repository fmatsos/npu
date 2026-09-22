//! CLI built-ins: `doctor`, `models`, `describe`, `version`, `update`, and the TCP
//! reachability probe they share.
//!
//! This module NEVER writes to the console itself: [`doctor`] returns
//! a report ([`Check`]) that the caller (`lib.rs::run`) formats with
//! [`format_doctor`] before writing it to stdout (contract rule: stdout
//! is reserved for the RESULT of a built-in, which IS its report). This is
//! what makes [`doctor`] fully testable without touching disk beyond
//! check (e), nor the network: the reachability probe (`probe`) is
//! injected by the caller rather than called directly, exactly like
//! `prompt::render`/`prompt::preflight` inject their environment
//! variable resolver (cf. `lib.rs::run`).
//!
//! **Deliberate omission: "NPU available".** This CLI is deliberately
//! agnostic of the inference runtime (the core only knows named backend
//! operations, never an NPU in the hardware sense): it therefore has NO way to
//! check the presence or availability of an NPU, unlike the
//! reachability of a backend (a TCP connection) or the validity of a
//! schema (a disk file read). Showing a checkmark for a check that
//! wasn't actually performed would be exactly the flaw that the L3
//! review architecture rule forbids ("a report that lies is worse than
//! no report"): [`doctor`] therefore NEVER produces a [`Check`] for
//! this line. This is not an oversight.

use std::net::ToSocketAddrs;

use serde::Serialize;

use crate::runtime::PROBE_TIMEOUT;

/// Outcome of a [`doctor`] check.
#[derive(Debug)]
pub enum Status {
    /// The check succeeded.
    Ok,
    /// The check failed, with an actionable message.
    Failed(String),
}

/// Category of a [`Check`], in the sense of the exit code from [`doctor_exit_code`]
/// (point 5 of the shared contract, spec §23): it is this value, and
/// NEVER the text of [`Check::label`], that distinguishes a
/// configuration failure ("fix your files") from a reachability failure
/// ("start your runtime") for a calling agent. A label is
/// display — it can be reworded, translated, or given a new
/// suffix without notice; the category is a machine contract and must
/// survive that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckKind {
    /// Checks (a), (c), (d), (e): configuration, models, commands.
    Config,
    /// Check (b): TCP reachability of a backend.
    Reachability,
}

/// One line of the [`doctor`] report.
#[derive(Debug)]
pub struct Check {
    /// Category of this check, used by [`doctor_exit_code`].
    pub kind: CheckKind,
    /// What was checked (e.g. `backend "ovms" reachable`).
    pub label: String,
    /// The result of this check.
    pub status: Status,
}

/// The built-in names exposed by this CLI, plus `help`, reserved by
/// `clap` itself (every `clap::Command` gets an automatic `-h`/`--help`
/// flag). `command.rs` (`reject_reserved_path`) rejects at load time
/// any command file whose FIRST path segment matches one of these
/// values (phase 5, point 2 of the shared contract): without this
/// rejection, `commands/doctor.md` would be silently shadowed by (or
/// would shadow) the `doctor` built-in built in `lib.rs`.
pub const RESERVED: &[&str] = &[
    "doctor", "models", "describe", "serve", "stop", "status", "logs", "version", "update", "help",
];

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
/// `probe` is injected (never [`tcp_probe`] called directly): this is what
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
/// EXHAUSTIVE work that phase 4 deliberately deferred to `doctor` (cf.
/// `output.rs`, `command::resolve_schema_path`: a schema's existence and
/// compilation are only checked, outside `doctor`, at the moment the
/// command that requires it is actually invoked). Reuses
/// `output::compile_schema` — made `pub(crate)` for this phase (the only
/// change allowed in `output.rs`, cf. the report) — rather than
/// writing a second implementation of schema compilation: the three
/// distinct error messages (absent/unreadable, invalid JSON, schema
/// syntactically invalid) it already produces are exactly the ones
/// this check must report.
///
/// A command without `[output].schema` (text format, or JSON without a
/// schema — both explicitly allowed by `command::convert_output`)
/// produces no [`Check`] here: there is nothing to check.
fn check_commands_output_schema(commands: &[crate::command::CommandSpec]) -> Vec<Check> {
    sorted_commands(commands)
        .into_iter()
        .filter_map(|spec| {
            let schema_path = spec.output.schema.as_deref()?;
            let path = spec.path.join("/");
            let status = match crate::output::compile_schema(schema_path, &spec.file) {
                Ok(_validator) => Status::Ok,
                Err(err) => Status::Failed(err.to_string()),
            };
            Some(Check {
                kind: CheckKind::Config,
                label: format!("command \"{path}\": output schema"),
                status,
            })
        })
        .collect()
}

/// Runs all `npu doctor` checks (point 3 of the shared contract) and
/// returns the report — without ever writing to the console or touching
/// the network (`probe` is injected); only check (e) touches disk, by
/// reading the declared schema files.
///
/// `config`/`commands` and `load_error` reflect the degraded mode of
/// `lib.rs::run` (shared-contract decision 1, corollary of the debt
/// tracked since phase 1): when loading fails, `run` KEEPS the error
/// instead of propagating it, so that `doctor` can still run and
/// report it as a failed check (a). This function does not assume that
/// `config`/`commands`/`load_error` vary as a block, though: each
/// family of checks (b/c, then d/e) only runs if the data it needs is
/// actually available, which stays correct whether the caller treats
/// loading as a single atomic operation (the case expected in
/// practice, cf. the report) or distinguishes a genuine configuration
/// failure from a command discovery failure.
#[must_use]
pub fn doctor(
    config: Option<&crate::config::Config>,
    commands: Option<&[crate::command::CommandSpec]>,
    load_error: Option<&crate::Error>,
    probe: &dyn Fn(&str) -> Result<(), String>,
    container_probe: &dyn Fn() -> Result<(), String>,
    runner: &dyn Fn(&[String]) -> crate::Result<String>,
) -> Vec<Check> {
    let mut checks = vec![check_config_loaded(load_error)];

    if let Some(config) = config {
        checks.extend(check_backends_reachable(config, probe, runner));
        checks.extend(check_container_runtime(config, container_probe));
        checks.extend(check_models(config));
    }

    if let (Some(config), Some(commands)) = (config, commands) {
        checks.extend(check_commands_model(config, commands));
        checks.extend(check_commands_output_schema(commands));
    }

    checks
}

/// Formats the `doctor` report for stdout (point 6 of the shared contract):
/// a checkmark (`✓`) followed by the label for each successful check, a
/// cross (`✗`) followed by the label THEN the message for each failure —
/// never the reverse, or the message explaining the failure would end up
/// without context on what it concerns.
#[must_use]
pub fn format_doctor(checks: &[Check]) -> String {
    checks
        .iter()
        .map(|check| match &check.status {
            Status::Ok => format!("✓ {}", check.label),
            Status::Failed(message) => format!("✗ {}: {message}", check.label),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Exit code of the `doctor` report (point 5 of the shared contract):
/// - `0` if all checks pass;
/// - `2` if at least one CONFIGURATION check (a, c, d, e) fails —
///   configuration takes priority, including when a reachability check
///   (b) fails at the same time;
/// - `3` if ONLY reachability (b) fails.
///
/// Distinguishes the two failure families via [`Check::kind`], never via
/// the text of [`Check::label`]: a label is display — it can be
/// reworded, translated, or given a new suffix without notice — while
/// the category is a machine contract (spec §23) that this exit code
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

/// Formats the `npu models` table (point 6 of the shared contract, spec
/// §16): NAME/BACKEND/OPERATION columns, sorted by name to stay
/// deterministic regardless of the underlying `HashMap`'s iteration
/// order, aligned to the ACTUAL width of the content (never a hardcoded
/// width: a model name longer than "NAME" widens its column). A
/// configuration with no models still produces the header — never an
/// empty table nor a panic.
#[must_use]
pub fn format_models(config: &crate::config::Config) -> String {
    const NAME_HEADER: &str = "NAME";
    const BACKEND_HEADER: &str = "BACKEND";
    const OPERATION_HEADER: &str = "OPERATION";
    // A routing key invisible here would be as good as silently ignored:
    // `npu models` is how one sees where a command actually goes.
    const FALLBACK_HEADER: &str = "FALLBACK";

    let mut rows: Vec<(&str, &str, &str, &str)> = config
        .models
        .values()
        .map(|model| {
            (
                model.id.as_str(),
                model.backend.as_str(),
                model.operation.as_str(),
                model.fallback.as_deref().unwrap_or("-"),
            )
        })
        .collect();
    rows.sort_unstable_by_key(|&(name, ..)| name);

    let name_width = rows
        .iter()
        .map(|&(name, ..)| name.len())
        .max()
        .unwrap_or(0)
        .max(NAME_HEADER.len());
    let backend_width = rows
        .iter()
        .map(|&(_name, backend, ..)| backend.len())
        .max()
        .unwrap_or(0)
        .max(BACKEND_HEADER.len());
    let operation_width = rows
        .iter()
        .map(|&(_name, _backend, operation, _fallback)| operation.len())
        .max()
        .unwrap_or(0)
        .max(OPERATION_HEADER.len());

    let mut lines = Vec::with_capacity(rows.len() + 1);
    lines.push(format!(
        "{NAME_HEADER:<name_width$}  {BACKEND_HEADER:<backend_width$}  \
         {OPERATION_HEADER:<operation_width$}  {FALLBACK_HEADER}"
    ));
    for (name, backend, operation, fallback) in rows {
        lines.push(format!(
            "{name:<name_width$}  {backend:<backend_width$}  \
             {operation:<operation_width$}  {fallback}"
        ));
    }

    lines.join("\n")
}

/// A declared argument as serialized by [`describe`]: same fields as
/// `command::ArgSpec`, never that one directly — `ArgSpec` only derives
/// `Deserialize` (it is never written to output elsewhere in this crate),
/// and this file is not allowed to modify `command.rs` to add
/// `Serialize` there (absolute rule of the task: exclusive owner of
/// `builtin.rs`).
#[derive(Serialize)]
struct DescribeArg<'a> {
    short: Option<char>,
    required: bool,
    description: &'a str,
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
}

/// The complete JSON description of a command, as serialized by
/// [`describe`].
#[derive(Serialize)]
struct Describe<'a> {
    name: String,
    description: &'a str,
    model: &'a str,
    input: &'a str,
    args: std::collections::BTreeMap<&'a str, DescribeArg<'a>>,
    output: DescribeOutput<'a>,
}

/// Describes a dynamically configured command (point 6 of the shared
/// contract, spec §16): produces JSON on stdout, serialized by
/// `serde_json` (never built by hand — a manual `format!` could not
/// correctly escape a description or a prompt containing quotes).
/// Extends the §16 example with what phases 3 and 4 added: the declared
/// arguments (`args`) and the output contract (`output`, with its
/// format, its schema if any, and its line limit if any).
///
/// Does NO resolution by name: the shared contract fixes this signature
/// to an already-resolved `CommandSpec` — it is up to the caller
/// (`lib.rs::run`) to look up this `CommandSpec` (with its own
/// `find_command`, already written and tested there) before calling
/// this function. See the report for the explicit decision behind this
/// choice: "describe on an unknown command" is therefore not a
/// behavior this module can produce or test, for lack of receiving a
/// name to resolve.
pub fn describe(spec: &crate::command::CommandSpec) -> crate::Result<String> {
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
                    description: arg_spec.description.as_str(),
                },
            )
        })
        .collect();

    let dto = Describe {
        name: spec.path.join("/"),
        description: &spec.description,
        model: &spec.model,
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
        },
    };

    serde_json::to_string(&dto)
        .map_err(|err| crate::Error::Config(format!("description serialization failed: {err}")))
}

/// Extracts `(host, port)` from a base URL "http(s)://host[:port][/...]"
/// (point 4 of the shared contract). Falls back to the scheme's implicit
/// port (80 for `http`, 443 for `https`) when no explicit port is
/// present. PURELY SYNTACTIC: never touches the network, only the
/// `base_url` string itself — it's [`tcp_probe`] that opens the
/// connection.
///
/// **Bracketed IPv6 notation** (`[::1]` or `[::1]:8000`, RFC 3986
/// §3.2.2) handled separately, BEFORE the general `rsplit_once(':')`: a
/// bare IPv6 address itself contains `:` characters, so a plain
/// `rsplit_once(':')` would cut `[::1]:8000` on the last `:` inside the
/// brackets rather than on the host/port separator. `host` is returned
/// WITHOUT the brackets (`"::1"`, not `"[::1]"`): `Ipv6Addr::from_str`,
/// used by `ToSocketAddrs` in [`tcp_probe`], rejects the bracketed form
/// — keeping it would make any IPv6 resolution fail with a spurious DNS
/// error, never with the invalid-port message one would expect.
fn parse_host_port(base_url: &str) -> Result<(String, u16), String> {
    let Some((scheme, rest)) = base_url.split_once("://") else {
        return Err(format!(
            "base_url \"{base_url}\": missing scheme (expected \"http://\" or \"https://\")"
        ));
    };

    let default_port: u16 = match scheme {
        "http" => 80,
        "https" => 443,
        other => {
            return Err(format!(
                "base_url \"{base_url}\": unsupported scheme \"{other}\" (expected \"http\" or \
                 \"https\")"
            ));
        }
    };

    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    if authority.is_empty() {
        return Err(format!("base_url \"{base_url}\": missing host"));
    }

    if let Some(after_bracket) = authority.strip_prefix('[') {
        return parse_ipv6_authority(base_url, after_bracket, default_port);
    }

    match authority.rsplit_once(':') {
        Some((host, port_text)) if !host.is_empty() => {
            let port: u16 = port_text
                .parse()
                .map_err(|_| format!("base_url \"{base_url}\": invalid port \"{port_text}\""))?;
            Ok((host.to_string(), port))
        }
        _ => Ok((authority.to_string(), default_port)),
    }
}

/// Complements [`parse_host_port`] for bracketed IPv6 notation:
/// `after_bracket` is what follows the opening `[` already consumed by
/// the caller (e.g. `"::1]:8000"` for `"[::1]:8000"`). Isolated in its
/// own function because [`parse_host_port`] already has two levels of
/// `match` / early-return; nesting it in place would hurt the
/// readability that `clippy::pedantic` (`too_many_lines`) would
/// otherwise penalize.
fn parse_ipv6_authority(
    base_url: &str,
    after_bracket: &str,
    default_port: u16,
) -> Result<(String, u16), String> {
    let Some(end) = after_bracket.find(']') else {
        return Err(format!(
            "base_url \"{base_url}\": unclosed opening IPv6 bracket \"[\""
        ));
    };
    // `end` points at `]` (ASCII, 1 byte): the two split bounds below
    // therefore always fall on a character boundary, whatever
    // `base_url`'s content around these brackets.
    let host = &after_bracket[..end];
    if host.is_empty() {
        return Err(format!(
            "base_url \"{base_url}\": empty IPv6 address between brackets"
        ));
    }
    let trailer = &after_bracket[end + 1..];

    match trailer.strip_prefix(':') {
        Some(port_text) if !port_text.is_empty() => port_text
            .parse()
            .map(|port| (host.to_string(), port))
            .map_err(|_| format!("base_url \"{base_url}\": invalid port \"{port_text}\"")),
        Some(_) => Err(format!("base_url \"{base_url}\": missing port after \":\"")),
        None if trailer.is_empty() => Ok((host.to_string(), default_port)),
        None => Err(format!(
            "base_url \"{base_url}\": unexpected characters after the IPv6 address \
             (\"{trailer}\")"
        )),
    }
}

/// Tests a backend's reachability with a TCP connection to its
/// `base_url` (point 4 of the shared contract), with a short timeout
/// ([`PROBE_TIMEOUT`]), then closes it immediately. NO HTTP request: a
/// `POST` on the `chat` operation would actually invoke the model, an
/// unacceptable side effect for a diagnostic command — this function
/// therefore only opens and closes a socket, never writing or reading a
/// single byte on it. The error message says "unreachable" on the
/// `doctor` side (via the label `"reachable"` — cf.
/// [`check_backends_reachable`]) and never "available": only a
/// socket's acceptance is checked, not the model's ability to respond.
///
/// # Errors
///
/// Returns `Err` if `base_url` does not have the expected shape, if
/// resolving the host/port pair fails, or if the connection itself
/// fails or times out.
pub fn tcp_probe(base_url: &str) -> Result<(), String> {
    let (host, port) = parse_host_port(base_url)?;

    let mut addrs = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|err| format!("address resolution for \"{host}:{port}\" failed: {err}"))?;

    let addr = addrs
        .next()
        .ok_or_else(|| format!("address resolution for \"{host}:{port}\" produced no address"))?;

    std::net::TcpStream::connect_timeout(&addr, PROBE_TIMEOUT)
        .map(|_stream| ())
        .map_err(|err| format!("TCP connection to \"{host}:{port}\" failed: {err}"))
}

/// One line of the [`status`] report: a backend declaring a runtime, and
/// the state that runtime is in.
struct RuntimeStatus {
    backend: String,
    /// Which runtime family manages it — `npu status` reports every family
    /// in one table, so a line that did not say which one it belongs to
    /// would be ambiguous the day a second family exists.
    runtime: String,
    /// What this family calls the thing it started: a container name for
    /// Docker. Named INSTANCE rather than CONTAINER because the column is
    /// shared by families that start no container at all.
    instance: String,
    /// The backend's resolved `base_url`. Reported because `port = "auto"`
    /// derives a number nothing else in the CLI shows: a value the user
    /// cannot read is only half a feature.
    url: String,
    state: String,
}

/// What [`status`] reports for a backend whose runtime does not exist.
/// Not an error: "not started" is a legitimate state of a lifecycle, and a
/// report that failed on it could never describe a stopped runtime.
const NOT_STARTED: &str = "not started";

/// What [`status`] shows in the URL column when a `port = "auto"` backend's
/// port cannot be read back — the runtime is not running yet.
const UNKNOWN_URL: &str = "-";

/// Resolves `model_id` to the model, its backend, and the runtime that
/// backend declares.
///
/// Shared by [`serve`], [`stop`] and [`logs`] so the three refuse the same
/// way: a model that does not exist, or a backend that was never told how to
/// start anything, is a configuration problem naming the identifier at
/// fault. Guessing a runtime from a `base_url` instead would let `npu stop`
/// act on a server npu never started.
fn lifecycle_target<'a>(
    config: &'a crate::config::Config,
    model_id: &str,
) -> crate::Result<(
    &'a crate::config::Model,
    &'a crate::config::Backend,
    &'a crate::config::Runtime,
)> {
    let (model, backend) = config.resolve(model_id)?;

    let Some(runtime) = backend.runtime() else {
        return Err(crate::Error::Config(format!(
            "backend \"{}\" (used by model \"{model_id}\") declares no [runtime] table: \
             npu only manages the runtimes it starts",
            backend.id
        )));
    };

    Ok((model, backend, runtime))
}

/// `npu serve <model>`: starts the runtime of the backend the model points
/// at, and returns the started instance's identifier — which IS this
/// command's result, hence what the caller writes to stdout.
///
/// A model names exactly one backend, so the model identifier alone is
/// unambiguous: several backends with a runtime coexist without anything to
/// disambiguate.
///
/// Orchestration only: resolve, `match` the family, delegate. Everything
/// that touches the outside world lives in `crate::runtime`, and `runner` is
/// injected exactly like `probe` in [`doctor`] — the tests of this module
/// never need Docker installed.
///
/// # Errors
///
/// - `Error::Config` if the model is unknown, if its backend is unknown, or
///   if that backend declares no runtime — in every case naming the
///   identifier at fault;
/// - whatever the runtime family returns otherwise (`Error::Backend` for the
///   real runner, cf. [`crate::runtime::docker::runner`]).
pub fn serve(
    config: &crate::config::Config,
    model_id: &str,
    env: &dyn Fn(&str) -> Option<String>,
    runner: &dyn Fn(&[String]) -> crate::Result<String>,
) -> crate::Result<String> {
    let (model, backend, runtime) = lifecycle_target(config, model_id)?;

    match runtime {
        crate::config::Runtime::Docker(docker) => {
            crate::runtime::docker::serve(backend, docker, model, env, runner)
        }
    }
}

/// `npu stop <model>`: tears down what [`serve`] started for that model's
/// backend, and returns the instance identifier — this command's result.
///
/// # Errors
///
/// Same as [`serve`].
pub fn stop(
    config: &crate::config::Config,
    model_id: &str,
    runner: &dyn Fn(&[String]) -> crate::Result<String>,
) -> crate::Result<String> {
    let (_model, backend, runtime) = lifecycle_target(config, model_id)?;

    match runtime {
        crate::config::Runtime::Docker(_) => crate::runtime::docker::stop(backend, runner),
    }
}

/// `npu logs <model>`: streams the runtime's logs.
///
/// Uses `streamer` and not `runner`: the logs ARE this command's result, and
/// a server writes them to both stdout and stderr. Capturing only stdout
/// would silently drop half of them.
///
/// # Errors
///
/// Same as [`serve`].
pub fn logs(
    config: &crate::config::Config,
    model_id: &str,
    follow: bool,
    streamer: &dyn Fn(&[String]) -> crate::Result<()>,
) -> crate::Result<()> {
    let (_model, backend, runtime) = lifecycle_target(config, model_id)?;

    match runtime {
        crate::config::Runtime::Docker(_) => {
            crate::runtime::docker::logs(backend, follow, streamer)
        }
    }
}

/// `npu status`: for every backend declaring a runtime, is that runtime up?
///
/// Backends are iterated sorted by identifier, like every other report in
/// this module: the report must not depend on the runtime's own ordering,
/// nor on a `HashMap`'s.
///
/// A backend the runtime cannot be asked about does not fail the command:
/// its state becomes the runtime's own message. `status` is a report, and a
/// report that dies on its first unknown line is not a report.
///
/// # Errors
///
/// Never returns `Err`; the signature stays `Result` for symmetry with the
/// other built-ins and so a future stricter mode does not break the caller.
pub fn status(
    config: &crate::config::Config,
    runner: &dyn Fn(&[String]) -> crate::Result<String>,
) -> crate::Result<String> {
    let mut ids: Vec<&String> = config.backends.keys().collect();
    ids.sort_unstable();

    let rows: Vec<RuntimeStatus> = ids
        .into_iter()
        .filter_map(|id| {
            // `id` comes from `config.backends`'s keys: the entry exists.
            let backend = &config.backends[id];
            // A backend with no runtime is not a line of this report: npu
            // was never told how to start it, so it has no state to show.
            let runtime = backend.runtime()?;

            let (instance, state) = match runtime {
                crate::config::Runtime::Docker(_) => (
                    crate::runtime::docker::container_name(&backend.id),
                    crate::runtime::docker::state(&backend.id, runner),
                ),
            };

            // A report that dies on its first unreadable line is not a
            // report: an unresolvable URL (runtime down, Docker absent)
            // becomes a dash, exactly like an absent FALLBACK in `models`.
            let url = crate::runtime::resolve_base_url(backend, runner)
                .unwrap_or_else(|_| UNKNOWN_URL.to_string());

            Some(RuntimeStatus {
                backend: id.clone(),
                runtime: crate::runtime::label(runtime).to_string(),
                instance,
                url,
                state: state.unwrap_or_else(|| NOT_STARTED.to_string()),
            })
        })
        .collect();

    Ok(format_status(&rows))
}

/// Formats the [`status`] table, columns sized to the content like
/// [`format_models`]. A configuration with no backend declaring a runtime
/// still produces the header: an empty output would be indistinguishable
/// from a command that did nothing.
fn format_status(rows: &[RuntimeStatus]) -> String {
    const BACKEND_HEADER: &str = "BACKEND";
    const RUNTIME_HEADER: &str = "RUNTIME";
    const INSTANCE_HEADER: &str = "INSTANCE";
    const URL_HEADER: &str = "URL";
    const STATE_HEADER: &str = "STATE";

    let width = |header: &str, of: &dyn Fn(&RuntimeStatus) -> usize| {
        rows.iter().map(of).max().unwrap_or(0).max(header.len())
    };
    let backend_width = width(BACKEND_HEADER, &|row: &RuntimeStatus| row.backend.len());
    let runtime_width = width(RUNTIME_HEADER, &|row: &RuntimeStatus| row.runtime.len());
    let instance_width = width(INSTANCE_HEADER, &|row: &RuntimeStatus| row.instance.len());
    let url_width = width(URL_HEADER, &|row: &RuntimeStatus| row.url.len());

    let mut lines = Vec::with_capacity(rows.len() + 1);
    lines.push(format!(
        "{BACKEND_HEADER:<backend_width$}  {RUNTIME_HEADER:<runtime_width$}  \
         {INSTANCE_HEADER:<instance_width$}  {URL_HEADER:<url_width$}  {STATE_HEADER}"
    ));
    for row in rows {
        lines.push(format!(
            "{:<backend_width$}  {:<runtime_width$}  {:<instance_width$}  {:<url_width$}  {}",
            row.backend, row.runtime, row.instance, row.url, row.state
        ));
    }

    lines.join("\n")
}

#[cfg(test)]
#[allow(clippy::expect_used)] // allowed in tests (cf. Cargo.toml [lints.clippy]).
#[allow(clippy::panic)] // a stub that must never be called says so by panicking.
mod tests {
    use super::*;

    fn backend(id: &str, base_url: &str, operations: &[&str]) -> crate::config::Backend {
        crate::config::Backend {
            id: id.to_string(),
            base_url: base_url.to_string(),
            kind: "openai-compatible".to_string(),
            operations: operations
                .iter()
                .map(|op| {
                    (
                        (*op).to_string(),
                        crate::config::Operation {
                            method: "POST".to_string(),
                            path: format!("/{op}"),
                        },
                    )
                })
                .collect(),
            port: None,
            runtime: None,
            docker: None,
            timeouts: None,
        }
    }

    /// Same as [`backend`], plus a Docker `[runtime]`: `options`, `image` and
    /// `args` are deliberately distinguishable so that a test can assert on
    /// their ORDER in the built command line.
    fn containerized_backend(id: &str) -> crate::config::Backend {
        let mut backend = backend(id, "http://127.0.0.1:8000", &["chat"]);
        backend.runtime = Some(crate::config::Runtime::Docker(crate::config::Docker {
            image: "example/server:latest".to_string(),
            options: vec![
                "-p".to_string(),
                "8000:8000".to_string(),
                "-v".to_string(),
                "{{ env.NPU_TEST_MODELS }}:/models".to_string(),
            ],
            args: vec!["--source_model".to_string(), "{{ args.model }}".to_string()],
        }));
        backend
    }

    /// The most common way anyone hits `serve` twice. Diagnosing it as a
    /// port conflict would send the user to edit a `port` key that is
    /// perfectly correct, so the container is checked FIRST.
    #[test]
    fn serve_on_an_already_served_backend_points_at_the_container_not_the_port() {
        let config = containerized_config();

        let err = serve(&config, "qwen", &test_env, &|args: &[String]| {
            assert_eq!(args.first().map(String::as_str), Some("ps"));
            Ok("npu-ovms\n".to_string())
        })
        .expect_err("an already-served backend must fail");

        assert_eq!(err.exit_code(), 3);
        let message = err.to_string();
        assert!(message.contains("npu-ovms"), "got: {message}");
        assert!(message.contains("qwen"), "got: {message}");
    }

    #[test]
    fn serve_reports_a_fixed_port_already_in_use_naming_the_backend() {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("binding the occupying listener");
        let port = listener.local_addr().expect("local address").port();

        let mut config = crate::config::Config::default();
        let mut backend = backend("ovms", &format!("http://127.0.0.1:{port}"), &["chat"]);
        backend.port = Some(crate::config::Port::Fixed(port));
        backend.runtime = Some(crate::config::Runtime::Docker(crate::config::Docker {
            image: "img".to_string(),
            options: vec![],
            args: vec![],
        }));
        config.backends.insert("ovms".to_string(), backend);
        config
            .models
            .insert("m".to_string(), model("m", "ovms", "chat"));

        let err = serve(&config, "m", &|_| None, &|args: &[String]| {
            assert_eq!(
                args.first().map(String::as_str),
                Some("ps"),
                "only the container probe may run when the port is already taken"
            );
            Ok(String::new())
        })
        .expect_err("an occupied fixed port must fail");

        assert_eq!(err.exit_code(), 3);
        let message = err.to_string();
        assert!(message.contains("ovms"), "got: {message}");
        assert!(message.contains(&port.to_string()), "got: {message}");
    }

    /// A runner no `doctor` test needs: every fixture uses a fixed port, so
    /// `resolve_base_url` returns before touching it. Calling it is the bug.
    fn unused_runner(_args: &[String]) -> crate::Result<String> {
        panic!("a fixed-port backend must never ask Docker for its port")
    }

    fn model(id: &str, backend: &str, operation: &str) -> crate::config::Model {
        crate::config::Model {
            id: id.to_string(),
            backend: backend.to_string(),
            operation: operation.to_string(),
            model: format!("{id}-underlying"),
            fallback: None,
            generation: crate::config::Generation::default(),
        }
    }

    fn command_spec(path: &[&str], model: &str) -> crate::command::CommandSpec {
        crate::command::CommandSpec {
            path: path.iter().map(ToString::to_string).collect(),
            description: String::new(),
            model: model.to_string(),
            input: crate::command::InputMode::Stdin,
            prompt: "{{ input }}".to_string(),
            args: std::collections::BTreeMap::new(),
            output: crate::output::OutputSpec::default(),
            file: std::path::PathBuf::new(),
        }
    }

    // `unnecessary_wraps`: these two functions are stub probes with a
    // FIXED signature (`&dyn Fn(&str) -> Result<(), String>`, cf.
    // `doctor`); they cannot be simplified without breaking that
    // signature.
    #[allow(clippy::unnecessary_wraps)]
    fn always_ok(_base_url: &str) -> Result<(), String> {
        Ok(())
    }

    fn always_fails(_base_url: &str) -> Result<(), String> {
        Err("connection refused".to_string())
    }

    // Same reasoning as `always_ok`/`always_fails`, for the container
    // runtime probe injected into `doctor` (signature `&dyn Fn() ->
    // Result<(), String>`): no test of this module ever needs Docker.
    #[allow(clippy::unnecessary_wraps)]
    fn container_ok() -> Result<(), String> {
        Ok(())
    }

    fn container_fails() -> Result<(), String> {
        Err("cannot run \"docker\"".to_string())
    }

    // -- doctor: nominal scenario -----------------------------------------

    #[test]
    fn doctor_all_checks_passing_yields_no_failure_and_exit_code_zero() {
        let mut config = crate::config::Config::default();
        config.backends.insert(
            "ovms".to_string(),
            backend("ovms", "http://127.0.0.1:8000", &["chat"]),
        );
        config
            .models
            .insert("qwen-fast".to_string(), model("qwen-fast", "ovms", "chat"));
        let commands = vec![command_spec(&["classify"], "qwen-fast")];

        let checks = doctor(
            Some(&config),
            Some(&commands),
            None,
            &always_ok,
            &container_ok,
            &unused_runner,
        );

        assert!(
            checks.iter().all(|c| matches!(c.status, Status::Ok)),
            "got: {checks:?}"
        );
        assert_eq!(doctor_exit_code(&checks), 0);
    }

    // -- (a) configuration loaded ------------------------------------------

    #[test]
    fn doctor_load_error_fails_configuration_check_only_and_exit_code_is_two() {
        let err = crate::Error::Config("broken command file".to_string());

        let checks = doctor(
            None,
            None,
            Some(&err),
            &always_ok,
            &container_ok,
            &unused_runner,
        );

        assert_eq!(
            checks.len(),
            1,
            "config/commands absent: only (a) must be produced, got: {checks:?}"
        );
        assert!(matches!(&checks[0].status, Status::Failed(_)));
        assert_eq!(doctor_exit_code(&checks), 2);
    }

    // -- (c) models ----------------------------------------------------------

    #[test]
    fn doctor_model_referencing_unknown_backend_fails_check_c_and_exit_code_is_two() {
        let mut config = crate::config::Config::default();
        config
            .models
            .insert("gpt".to_string(), model("gpt", "does-not-exist", "chat"));

        let checks = doctor(
            Some(&config),
            Some(&[]),
            None,
            &always_ok,
            &container_ok,
            &unused_runner,
        );

        let model_check = checks
            .iter()
            .find(|c| c.label.contains("gpt"))
            .expect("a check for model \"gpt\" must exist");
        assert!(matches!(&model_check.status, Status::Failed(_)));
        assert_eq!(doctor_exit_code(&checks), 2);
    }

    #[test]
    fn doctor_model_referencing_unexposed_operation_fails_check_c() {
        let mut config = crate::config::Config::default();
        config.backends.insert(
            "ovms".to_string(),
            backend("ovms", "http://127.0.0.1:8000", &["chat"]),
        );
        config.models.insert(
            "whisper".to_string(),
            model("whisper", "ovms", "audio_transcriptions"),
        );

        let checks = doctor(
            Some(&config),
            Some(&[]),
            None,
            &always_ok,
            &container_ok,
            &unused_runner,
        );

        let model_check = checks
            .iter()
            .find(|c| c.label.contains("whisper"))
            .expect("a check for model \"whisper\" must exist");
        assert!(matches!(
            &model_check.status,
            Status::Failed(message) if message.contains("audio_transcriptions")
        ));
        assert_eq!(doctor_exit_code(&checks), 2);
    }

    // -- (d) commands / model ------------------------------------------------

    #[test]
    fn doctor_command_referencing_unknown_model_fails_check_d() {
        let config = crate::config::Config::default();
        let commands = vec![command_spec(&["classify"], "does-not-exist")];

        let checks = doctor(
            Some(&config),
            Some(&commands),
            None,
            &always_ok,
            &container_ok,
            &unused_runner,
        );

        let command_check = checks
            .iter()
            .find(|c| c.label.contains("classify") && c.label.contains("model"))
            .expect("a check (d) for \"classify\" must exist");
        assert!(matches!(&command_check.status, Status::Failed(_)));
        assert_eq!(doctor_exit_code(&checks), 2);
    }

    // -- (e) commands / output schema ---------------------------------------

    #[test]
    fn doctor_command_with_missing_schema_file_fails_check_e() {
        let mut spec = command_spec(&["classify"], "qwen-fast");
        spec.output = crate::output::OutputSpec {
            format: crate::output::Format::Json,
            schema: Some(std::path::PathBuf::from(
                "/does/not/exist/schema-not-found.json",
            )),
            max_lines: None,
        };
        let config = crate::config::Config::default();

        let checks = doctor(
            Some(&config),
            Some(&[spec]),
            None,
            &always_ok,
            &container_ok,
            &unused_runner,
        );

        let schema_check = checks
            .iter()
            .find(|c| c.label.contains("schema"))
            .expect("a check (e) must exist");
        assert!(matches!(&schema_check.status, Status::Failed(_)));
    }

    #[test]
    fn doctor_command_without_schema_produces_no_check_e() {
        let spec = command_spec(&["commit-message"], "qwen-fast"); // text format by default
        let config = crate::config::Config::default();

        let checks = doctor(
            Some(&config),
            Some(&[spec]),
            None,
            &always_ok,
            &container_ok,
            &unused_runner,
        );

        assert!(
            !checks.iter().any(|c| c.label.contains("schema")),
            "no check (e) must be produced in the absence of a schema, got: {checks:?}"
        );
    }

    // -- exit code: configuration takes priority over reachability -----

    #[test]
    fn doctor_reachability_failure_alone_yields_exit_code_three() {
        let mut config = crate::config::Config::default();
        config.backends.insert(
            "ovms".to_string(),
            backend("ovms", "http://127.0.0.1:8000", &["chat"]),
        );

        let checks = doctor(
            Some(&config),
            Some(&[]),
            None,
            &always_fails,
            &container_ok,
            &unused_runner,
        );

        assert_eq!(doctor_exit_code(&checks), 3);
    }

    /// Regression: classification must come from [`CheckKind`], never
    /// from the label text. A reachability label deliberately reworded,
    /// without the word "reachable" nor the historical suffix, must
    /// still produce exit code 3 — text-based classification silently
    /// reclassified this case as a configuration failure (code 2).
    #[test]
    fn doctor_exit_code_uses_kind_not_label_text_for_reachability_failure() {
        let checks = vec![Check {
            kind: CheckKind::Reachability,
            label: "backend \"ovms\" status".to_string(),
            status: Status::Failed("connection refused".to_string()),
        }];

        assert_eq!(doctor_exit_code(&checks), 3);
    }

    #[test]
    fn doctor_config_failure_takes_priority_over_reachability_failure() {
        let mut config = crate::config::Config::default();
        config.backends.insert(
            "ovms".to_string(),
            backend("ovms", "http://127.0.0.1:8000", &["chat"]),
        );
        // (c) fails: the model references a nonexistent backend.
        config.models.insert(
            "orphan".to_string(),
            model("orphan", "does-not-exist", "chat"),
        );

        // (b) also fails: the probe fails for every backend queried.
        let checks = doctor(
            Some(&config),
            Some(&[]),
            None,
            &always_fails,
            &container_ok,
            &unused_runner,
        );

        let reachability_failed = checks
            .iter()
            .any(|c| c.kind == CheckKind::Reachability && matches!(c.status, Status::Failed(_)));
        let config_check_failed = checks
            .iter()
            .any(|c| c.label.contains("orphan") && matches!(c.status, Status::Failed(_)));
        assert!(
            reachability_failed,
            "precondition: the probe must have failed"
        );
        assert!(
            config_check_failed,
            "precondition: check (c) must have failed"
        );

        assert_eq!(
            doctor_exit_code(&checks),
            2,
            "configuration must take priority over reachability (point 5 of the shared contract)"
        );
    }

    // -- format_doctor -----------------------------------------------------------

    #[test]
    fn format_doctor_uses_checkmark_for_ok_and_cross_with_message_for_failed() {
        let checks = vec![
            Check {
                kind: CheckKind::Config,
                label: "a".to_string(),
                status: Status::Ok,
            },
            Check {
                kind: CheckKind::Config,
                label: "b".to_string(),
                status: Status::Failed("boom".to_string()),
            },
        ];

        assert_eq!(format_doctor(&checks), "✓ a\n✗ b: boom");
    }

    // -- format_models -------------------------------------------------------------

    #[test]
    fn format_models_aligns_backend_column_across_rows_of_differing_name_length() {
        let mut config = crate::config::Config::default();
        config
            .models
            .insert("a".to_string(), model("a", "ovms", "chat"));
        config.models.insert(
            "much-longer-model-name".to_string(),
            model("much-longer-model-name", "ovms", "chat"),
        );

        let out = format_models(&config);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3);

        let header_backend_at = lines[0].find("BACKEND").expect("BACKEND header");
        let row_a_backend_at = lines[1].find("ovms").expect("row \"a\"");
        let row_long_backend_at = lines[2].find("ovms").expect("row \"much-longer…\"");

        assert_eq!(header_backend_at, row_a_backend_at);
        assert_eq!(header_backend_at, row_long_backend_at);
    }

    #[test]
    fn format_models_sorts_deterministically_by_name() {
        let mut config = crate::config::Config::default();
        config
            .models
            .insert("zebra".to_string(), model("zebra", "ovms", "chat"));
        config
            .models
            .insert("alpha".to_string(), model("alpha", "ovms", "chat"));

        let out = format_models(&config);
        let lines: Vec<&str> = out.lines().collect();

        let alpha_at = lines
            .iter()
            .position(|l| l.starts_with("alpha"))
            .expect("\"alpha\" must be present");
        let zebra_at = lines
            .iter()
            .position(|l| l.starts_with("zebra"))
            .expect("\"zebra\" must be present");
        assert!(alpha_at < zebra_at);
    }

    #[test]
    fn format_models_with_no_models_prints_only_the_header_without_panicking() {
        let config = crate::config::Config::default();

        let out = format_models(&config);

        assert_eq!(out, "NAME  BACKEND  OPERATION  FALLBACK");
    }

    // -- describe --------------------------------------------------------------

    fn sample_translate_spec() -> crate::command::CommandSpec {
        let mut args = std::collections::BTreeMap::new();
        args.insert(
            "language".to_string(),
            crate::command::ArgSpec {
                short: Some('l'),
                required: true,
                description: "Target language".to_string(),
            },
        );
        crate::command::CommandSpec {
            path: vec!["translate".to_string()],
            description: "Translate input text".to_string(),
            model: "qwen-fast".to_string(),
            input: crate::command::InputMode::StdinOrFile,
            prompt: "Translate {{ input }} to {{ args.language }}".to_string(),
            args,
            output: crate::output::OutputSpec {
                format: crate::output::Format::Json,
                schema: Some(std::path::PathBuf::from("schemas/translation.json")),
                max_lines: None,
            },
            file: std::path::PathBuf::from(".npu/commands/translate.md"),
        }
    }

    #[test]
    fn describe_produces_valid_json_with_declared_args_and_output_contract() {
        let spec = sample_translate_spec();

        let json_text = describe(&spec).expect("describe must succeed");
        let value: serde_json::Value =
            serde_json::from_str(&json_text).expect("describe must produce valid JSON");

        assert_eq!(value["name"], "translate");
        assert_eq!(value["description"], "Translate input text");
        assert_eq!(value["model"], "qwen-fast");
        assert_eq!(value["input"], "stdin_or_file");
        assert_eq!(value["args"]["language"]["short"], "l");
        assert_eq!(value["args"]["language"]["required"], true);
        assert_eq!(value["args"]["language"]["description"], "Target language");
        assert_eq!(value["output"]["format"], "json");
        assert_eq!(value["output"]["schema"], "schemas/translation.json");
    }

    #[test]
    fn describe_text_output_without_schema_has_a_null_schema_field() {
        let mut spec = sample_translate_spec();
        spec.output = crate::output::OutputSpec {
            format: crate::output::Format::Text,
            schema: None,
            max_lines: Some(1),
        };

        let json_text = describe(&spec).expect("describe must succeed");
        let value: serde_json::Value =
            serde_json::from_str(&json_text).expect("describe must produce valid JSON");

        assert_eq!(value["output"]["format"], "text");
        assert!(value["output"]["schema"].is_null());
        assert_eq!(value["output"]["max_lines"], 1);
    }

    // -- parse_host_port -------------------------------------------------------

    #[test]
    fn parse_host_port_defaults_http_to_port_80() {
        assert_eq!(
            parse_host_port("http://127.0.0.1").expect("must parse"),
            ("127.0.0.1".to_string(), 80)
        );
    }

    #[test]
    fn parse_host_port_defaults_https_to_port_443() {
        assert_eq!(
            parse_host_port("https://example.com").expect("must parse"),
            ("example.com".to_string(), 443)
        );
    }

    #[test]
    fn parse_host_port_explicit_port_is_used() {
        assert_eq!(
            parse_host_port("http://127.0.0.1:8000").expect("must parse"),
            ("127.0.0.1".to_string(), 8000)
        );
    }

    #[test]
    fn parse_host_port_tolerates_a_path_after_the_authority() {
        assert_eq!(
            parse_host_port("http://127.0.0.1:8000/v3/chat").expect("must parse"),
            ("127.0.0.1".to_string(), 8000)
        );
    }

    #[test]
    fn parse_host_port_missing_scheme_is_an_error() {
        assert!(parse_host_port("127.0.0.1:8000").is_err());
    }

    #[test]
    fn parse_host_port_unsupported_scheme_is_an_error() {
        assert!(parse_host_port("ftp://127.0.0.1:21").is_err());
    }

    // -- parse_host_port: bracketed IPv6 ----------------------------------

    #[test]
    fn parse_host_port_ipv6_with_explicit_port_strips_brackets_from_host() {
        // The returned host must be WITHOUT brackets: `Ipv6Addr::from_str`
        // (used by `ToSocketAddrs` in `tcp_probe`) rejects the bracketed
        // form, cf. `parse_ipv6_authority`'s doc.
        assert_eq!(
            parse_host_port("http://[::1]:8000").expect("must parse"),
            ("::1".to_string(), 8000)
        );
    }

    #[test]
    fn parse_host_port_ipv6_without_port_defaults_to_scheme_port() {
        assert_eq!(
            parse_host_port("https://[2001:db8::1]").expect("must parse"),
            ("2001:db8::1".to_string(), 443)
        );
    }

    #[test]
    fn parse_host_port_ipv6_tolerates_a_path_after_the_authority() {
        assert_eq!(
            parse_host_port("http://[::1]:8000/v3/chat").expect("must parse"),
            ("::1".to_string(), 8000)
        );
    }

    #[test]
    fn parse_host_port_ipv6_unclosed_bracket_is_an_error_not_a_panic() {
        assert!(parse_host_port("http://[::1:8000").is_err());
    }

    #[test]
    fn parse_host_port_ipv6_empty_brackets_is_an_error() {
        assert!(parse_host_port("http://[]:8000").is_err());
    }

    #[test]
    fn parse_host_port_ipv6_trailing_colon_without_port_is_an_error() {
        assert!(parse_host_port("http://[::1]:").is_err());
    }

    #[test]
    fn parse_host_port_ipv6_garbage_after_bracket_is_an_error_not_a_panic() {
        assert!(parse_host_port("http://[::1]garbage").is_err());
    }

    #[test]
    fn parse_host_port_ipv6_non_numeric_port_is_an_error() {
        assert!(parse_host_port("http://[::1]:notaport").is_err());
    }

    // -- tcp_probe --------------------------------------------------------------

    #[test]
    fn tcp_probe_succeeds_against_a_locally_bound_listener() {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind of the ephemeral listener");
        let addr = listener.local_addr().expect("listener's local address");
        let base_url = format!("http://{addr}");

        // Accepts the incoming connection then finishes: no thread nor
        // socket must survive this test.
        let acceptor = std::thread::spawn(move || {
            let _ = listener.accept();
        });

        let result = tcp_probe(&base_url);

        acceptor.join().expect("the acceptor thread must not panic");
        assert!(result.is_ok(), "got: {result:?}");
    }

    #[test]
    fn tcp_probe_fails_against_a_closed_port() {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind of the ephemeral listener");
        let addr = listener.local_addr().expect("listener's local address");
        drop(listener); // closes immediately: nobody is listening here anymore.

        let result = tcp_probe(&format!("http://{addr}"));

        assert!(
            result.is_err(),
            "a closed port must be reported as unreachable"
        );
    }

    // -- serve -------------------------------------------------------------

    /// Stub runner: records the argument list it was given, and returns a
    /// container identifier. `serve` never spawns a process itself, so no
    /// test of this module needs Docker installed.
    fn capturing_runner(
        captured: &std::cell::RefCell<Vec<String>>,
    ) -> impl Fn(&[String]) -> crate::Result<String> + '_ {
        move |args: &[String]| {
            // `serve` asks "does this container already exist?" before
            // starting anything; answering with a container name would make
            // every fixture look already-served. Empty means absent, and the
            // probe is not what these tests capture.
            if args.first().is_some_and(|verb| verb == "ps") {
                return Ok(String::new());
            }
            captured.replace(args.to_vec());
            Ok("3f2a9c1b8e40".to_string())
        }
    }

    fn test_env(name: &str) -> Option<String> {
        match name {
            "NPU_TEST_MODELS" => Some("/home/tester/models".to_string()),
            _ => None,
        }
    }

    fn containerized_config() -> crate::config::Config {
        let mut config = crate::config::Config::default();
        config
            .backends
            .insert("ovms".to_string(), containerized_backend("ovms"));
        config
            .models
            .insert("qwen".to_string(), model("qwen", "ovms", "chat"));
        config
    }

    #[test]
    fn serve_builds_docker_run_with_options_then_image_then_args() {
        let config = containerized_config();
        let captured = std::cell::RefCell::new(Vec::new());

        let id = serve(&config, "qwen", &test_env, &capturing_runner(&captured))
            .expect("serving a containerized backend must succeed");

        assert_eq!(id, "3f2a9c1b8e40");
        assert_eq!(
            captured.into_inner(),
            vec![
                "run",
                "-d",
                "--name",
                "npu-ovms",
                "-p",
                "8000:8000",
                "-v",
                "/home/tester/models:/models",
                "example/server:latest",
                "--source_model",
                "qwen-underlying",
            ]
        );
    }

    #[test]
    fn serve_unknown_model_is_a_config_error_naming_the_model() {
        let config = containerized_config();
        let captured = std::cell::RefCell::new(Vec::new());

        let err = serve(&config, "absent", &test_env, &capturing_runner(&captured))
            .expect_err("an unknown model must fail");

        assert!(matches!(err, crate::Error::Config(_)));
        assert!(
            err.to_string().contains("absent"),
            "the message must name the faulty model"
        );
        assert!(
            captured.into_inner().is_empty(),
            "no container must be started for an unknown model"
        );
    }

    #[test]
    fn serve_backend_without_docker_table_is_a_config_error_naming_the_backend() {
        let mut config = crate::config::Config::default();
        config.backends.insert(
            "plain".to_string(),
            backend("plain", "http://127.0.0.1:8000", &["chat"]),
        );
        config
            .models
            .insert("qwen".to_string(), model("qwen", "plain", "chat"));
        let captured = std::cell::RefCell::new(Vec::new());

        let err = serve(&config, "qwen", &test_env, &capturing_runner(&captured))
            .expect_err("a backend without [docker] cannot be served");

        assert!(matches!(err, crate::Error::Config(_)));
        assert!(
            err.to_string().contains("plain"),
            "the message must name the backend that cannot be started"
        );
        assert!(captured.into_inner().is_empty());
    }

    #[test]
    fn serve_propagates_the_runner_failure() {
        let config = containerized_config();
        let failing = |_args: &[String]| Err(crate::Error::Backend("docker absent".to_string()));

        let err = serve(&config, "qwen", &test_env, &failing)
            .expect_err("a failing runner must fail the command");

        assert!(matches!(err, crate::Error::Backend(_)));
        assert_eq!(err.exit_code(), 3);
    }

    #[test]
    fn serve_undefined_environment_variable_is_a_config_error_naming_it() {
        let config = containerized_config();
        let empty_env = |_name: &str| None;
        let captured = std::cell::RefCell::new(Vec::new());

        let err = serve(&config, "qwen", &empty_env, &capturing_runner(&captured))
            .expect_err("an undefined variable referenced by [docker] must fail");

        assert!(matches!(err, crate::Error::Config(_)));
        assert!(
            err.to_string().contains("NPU_TEST_MODELS"),
            "the message must name the undefined variable"
        );
        assert!(captured.into_inner().is_empty());
    }

    // -- doctor: check (f), container runtime ------------------------------

    #[test]
    fn doctor_produces_no_container_check_when_no_backend_declares_docker() {
        let mut config = crate::config::Config::default();
        config.backends.insert(
            "plain".to_string(),
            backend("plain", "http://127.0.0.1:8000", &["chat"]),
        );

        let checks = doctor(
            Some(&config),
            Some(&[]),
            None,
            &always_ok,
            &container_fails,
            &unused_runner,
        );

        assert!(
            checks
                .iter()
                .all(|check| matches!(check.status, Status::Ok)),
            "a configuration without [docker] must not be penalized by a missing runtime"
        );
        assert_eq!(doctor_exit_code(&checks), 0);
    }

    #[test]
    fn doctor_container_runtime_failure_is_reachability_and_yields_exit_code_three() {
        let config = containerized_config();

        let checks = doctor(
            Some(&config),
            Some(&[]),
            None,
            &always_ok,
            &container_fails,
            &unused_runner,
        );

        let failed: Vec<&Check> = checks
            .iter()
            .filter(|check| matches!(check.status, Status::Failed(_)))
            .collect();
        assert_eq!(failed.len(), 1, "only the container check must fail");
        assert_eq!(failed[0].kind, CheckKind::Reachability);
        assert_eq!(doctor_exit_code(&checks), 3);
    }

    #[test]
    fn doctor_container_runtime_available_passes_when_a_backend_declares_docker() {
        let config = containerized_config();

        let checks = doctor(
            Some(&config),
            Some(&[]),
            None,
            &always_ok,
            &container_ok,
            &unused_runner,
        );

        assert!(
            checks
                .iter()
                .any(|check| check.kind == CheckKind::Reachability
                    && check.label.contains("container")),
            "a declared [docker] table must produce its own check"
        );
        assert_eq!(doctor_exit_code(&checks), 0);
    }

    // -- lifecycle: stop / status / logs -----------------------------------

    #[test]
    fn stop_removes_the_container_named_after_the_backend() {
        let config = containerized_config();
        let captured = std::cell::RefCell::new(Vec::new());

        stop(&config, "qwen", &capturing_runner(&captured)).expect("stopping must succeed");

        assert_eq!(
            captured.into_inner(),
            vec!["rm", "--force", "npu-ovms"],
            "stop must REMOVE the container: a stopped one still owns its name"
        );
    }

    #[test]
    fn stop_returns_the_container_name_when_the_runtime_printed_nothing() {
        let config = containerized_config();
        // `docker rm --force` on an absent container succeeds without
        // printing: stdout must still carry a result, never a blank line.
        let silent = |_args: &[String]| Ok(String::new());

        let result = stop(&config, "qwen", &silent).expect("stopping must stay idempotent");

        assert_eq!(result, "npu-ovms");
    }

    #[test]
    fn stop_backend_without_docker_table_is_a_config_error_naming_the_backend() {
        let mut config = crate::config::Config::default();
        config.backends.insert(
            "plain".to_string(),
            backend("plain", "http://127.0.0.1:8000", &["chat"]),
        );
        config
            .models
            .insert("qwen".to_string(), model("qwen", "plain", "chat"));
        let captured = std::cell::RefCell::new(Vec::new());

        let err = stop(&config, "qwen", &capturing_runner(&captured))
            .expect_err("npu only manages the containers it starts");

        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("plain"));
        assert!(captured.into_inner().is_empty());
    }

    #[test]
    fn logs_follow_adds_the_flag_and_streams_without_capturing() {
        let config = containerized_config();
        let captured = std::cell::RefCell::new(Vec::new());
        let streamer = |args: &[String]| {
            captured.replace(args.to_vec());
            Ok(())
        };

        logs(&config, "qwen", true, &streamer).expect("streaming must succeed");

        assert_eq!(captured.into_inner(), vec!["logs", "--follow", "npu-ovms"]);
    }

    #[test]
    fn logs_without_follow_omits_the_flag() {
        let config = containerized_config();
        let captured = std::cell::RefCell::new(Vec::new());
        let streamer = |args: &[String]| {
            captured.replace(args.to_vec());
            Ok(())
        };

        logs(&config, "qwen", false, &streamer).expect("streaming must succeed");

        assert_eq!(captured.into_inner(), vec!["logs", "npu-ovms"]);
    }

    #[test]
    fn status_reports_a_containerized_backend_with_the_runtime_state() {
        let config = containerized_config();
        let runner = |_args: &[String]| Ok("Up 3 minutes".to_string());

        let report = status(&config, &runner).expect("status never fails");

        assert!(report.contains("ovms"), "got: {report}");
        assert!(report.contains("npu-ovms"), "got: {report}");
        assert!(report.contains("Up 3 minutes"), "got: {report}");
    }

    /// One table reports every family, so each line must say which family
    /// it belongs to — otherwise a PID and a container name share a column
    /// with nothing to tell them apart.
    #[test]
    fn status_names_the_runtime_family_of_each_line() {
        let config = containerized_config();
        let runner = |_args: &[String]| Ok("Up 3 minutes".to_string());

        let report = status(&config, &runner).expect("status never fails");

        assert!(
            report.contains(crate::runtime::docker::NAME),
            "got: {report}"
        );
    }

    #[test]
    fn status_reports_a_missing_container_as_not_started_rather_than_failing() {
        let config = containerized_config();
        let runner = |_args: &[String]| Ok(String::new());

        let report =
            status(&config, &runner).expect("an absent container is a state, not an error");

        assert!(report.contains(NOT_STARTED), "got: {report}");
    }

    #[test]
    fn status_survives_a_runtime_that_cannot_be_asked() {
        let config = containerized_config();
        let runner = |_args: &[String]| Err(crate::Error::Backend("docker absent".to_string()));

        let report = status(&config, &runner).expect("a report must not die on its first bad line");

        assert!(report.contains("ovms"), "got: {report}");
    }

    #[test]
    fn status_ignores_backends_without_a_docker_table_but_keeps_its_header() {
        let mut config = crate::config::Config::default();
        config.backends.insert(
            "plain".to_string(),
            backend("plain", "http://127.0.0.1:8000", &["chat"]),
        );
        let runner = |_args: &[String]| Ok("Up".to_string());

        let report = status(&config, &runner).expect("status never fails");

        assert!(!report.contains("plain"), "got: {report}");
        assert!(
            report.contains("BACKEND"),
            "the header must survive an empty table"
        );
    }
}

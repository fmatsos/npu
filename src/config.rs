//! Configuration loading: backends and models.
//!
//! `load` loads a single scope. `load_scopes` composes several scopes in
//! layers: replacement by `id`, with the most local scope winning entirely —
//! no field-by-field merge.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// An HTTP operation exposed by a backend (e.g. `chat`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Operation {
    pub method: String,
    pub path: String,
}

/// How to start the runtime of a backend as a container, declared by the
/// optional `[docker]` table of `backends/*.toml`.
///
/// `options` and `args` are two lists rather than one because `docker run`'s
/// own grammar imposes it (`docker run [OPTIONS] IMAGE [ARG...]`): merging
/// them would make the position of the image implicit.
///
/// Both lists go through `crate::prompt::render` (`{{ args.model }}`,
/// `{{ env.NAME }}`) — the CLI embeds no container knowledge beyond the
/// shape of a `docker run` invocation.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Docker {
    pub image: String,
    /// Passed to `docker run` BEFORE the image (ports, volumes, devices).
    #[serde(default)]
    pub options: Vec<String>,
    /// Passed to the image AFTER it (the server's own arguments).
    #[serde(default)]
    pub args: Vec<String>,
}

/// Per-backend request timeout override, declared by the optional
/// `[timeouts]` table of `backends/*.toml`. Absent, `backend.rs` falls back
/// to its own default.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Timeouts {
    /// Seconds granted to a request before failure. Must be non-zero: a
    /// backend that declares `[timeouts]` without meaning to bound anything
    /// should omit the table instead.
    pub request_secs: u64,
}

/// The value of a backend's optional `port` key: an explicit number, or the
/// keyword `"auto"`.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Port {
    Fixed(u16),
    /// Any string; only `"auto"` is accepted, checked by [`resolve_port`]
    /// so that a typo names its file instead of being treated as an unknown
    /// type by serde.
    Keyword(String),
}

/// The keyword accepted by `port`.
const PORT_AUTO: &str = "auto";

/// The value substituted for `{{ backend.port }}` in the `[docker]` lists of
/// a `port = "auto"` backend: Docker's own "pick a free one".
///
/// Allocation is delegated to the kernel rather than derived or probed
/// because `npu` keeps no state between processes: `npu serve` and the
/// `npu <command>` that follows minutes later, in another process, must
/// agree on a port. Deriving one (a hash of the id) agrees but can collide
/// with an unrelated service; probing for a free one does not agree at all,
/// since by request time the port is occupied — by us. Letting Docker
/// allocate and then ASKING it what it allocated is the only variant that is
/// both collision-free and reproducible, at the cost of making Docker a
/// prerequisite for executing commands on such a backend, not just for its
/// lifecycle.
pub(crate) const DOCKER_EPHEMERAL_PORT: u16 = 0;

/// The only `{{ backend.<name> }}` placeholder that exists.
const BACKEND_PLACEHOLDER: &str = "backend.port";

/// Replaces every `{{ backend.port }}` in `template` with `port`, and reports
/// whether it replaced anything.
///
/// Handled HERE rather than through `prompt.rs`: this placeholder has no
/// meaning in a command file, so the prompt engine stays unaware of it and
/// `validate_docker_template` stays as it is. A fixed `port` is substituted
/// at load time; a `port = "auto"` base URL keeps its placeholder until
/// `builtin::resolve_base_url` reads the real value back from Docker.
pub(crate) fn substitute_port(template: &str, port: u16) -> (String, bool) {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    let mut substituted = false;

    while let Some(open) = rest.find("{{") {
        let after_open = &rest[open + 2..];
        let Some(close) = after_open.find("}}") else {
            break;
        };
        if after_open[..close].trim() != BACKEND_PLACEHOLDER {
            out.push_str(&rest[..open + 2]);
            rest = after_open;
            continue;
        }
        out.push_str(&rest[..open]);
        out.push_str(&port.to_string());
        rest = &after_open[close + 2..];
        substituted = true;
    }

    out.push_str(rest);
    (out, substituted)
}

/// Substitutes `{{ backend.port }}` throughout a backend, and enforces that
/// `port` and the placeholder are declared together.
///
/// Both directions are configuration errors naming the file: a placeholder
/// without a `port` cannot be resolved, and a `port` no placeholder reads is
/// a key that would be silently ignored.
fn resolve_port(backend: &mut Backend, source: &Path) -> crate::Result<()> {
    let references_port = |backend: &Backend| {
        substitute_port(&backend.base_url, 0).1
            || backend.docker.as_ref().is_some_and(|docker| {
                std::iter::once(&docker.image)
                    .chain(&docker.options)
                    .chain(&docker.args)
                    .any(|template| substitute_port(template, 0).1)
            })
    };

    let port = match &backend.port {
        None => {
            if references_port(backend) {
                return Err(crate::Error::Config(format!(
                    "{}: backend \"{}\": references {{{{ {BACKEND_PLACEHOLDER} }}}} but \
                     declares no \"port\" key",
                    source.display(),
                    backend.id
                )));
            }
            return Ok(());
        }
        Some(Port::Fixed(0)) => {
            return Err(crate::Error::Config(format!(
                "{}: backend \"{}\": port 0 is not a port (\"{PORT_AUTO}\" lets Docker \
                 allocate one instead)",
                source.display(),
                backend.id
            )));
        }
        Some(Port::Fixed(number)) => *number,
        Some(Port::Keyword(keyword)) if keyword == PORT_AUTO => {
            // `auto` means "Docker allocates, npu asks it back", so both
            // halves must exist: something to start, and a base URL whose
            // port can be filled in afterwards.
            if backend.docker.is_none() {
                return Err(crate::Error::Config(format!(
                    "{}: backend \"{}\": port = \"{PORT_AUTO}\" requires a [docker] table — \
                     npu can only read back a port it asked Docker to allocate",
                    source.display(),
                    backend.id
                )));
            }
            // BOTH sides must read the placeholder, for the same reason:
            // without it in [docker] nothing is published, and without it in
            // base_url nothing reaches what was published. Either way the
            // allocated port is unreachable — a failure that would only
            // surface at the first command, as advice ("start it with npu
            // serve") that could never work.
            let in_docker = backend.docker.as_ref().is_some_and(|docker| {
                std::iter::once(&docker.image)
                    .chain(&docker.options)
                    .chain(&docker.args)
                    .any(|template| substitute_port(template, 0).1)
            });
            if !substitute_port(&backend.base_url, 0).1 || !in_docker {
                return Err(crate::Error::Config(format!(
                    "{}: backend \"{}\": port = \"{PORT_AUTO}\" requires \
                     {{{{ {BACKEND_PLACEHOLDER} }}}} in BOTH base_url and [docker], otherwise \
                     the allocated port is never published or never reached",
                    source.display(),
                    backend.id
                )));
            }
            DOCKER_EPHEMERAL_PORT
        }
        Some(Port::Keyword(keyword)) => {
            return Err(crate::Error::Config(format!(
                "{}: backend \"{}\": port \"{keyword}\" is neither a number nor \
                 \"{PORT_AUTO}\"",
                source.display(),
                backend.id
            )));
        }
    };

    let mut used = false;

    // A `port = "auto"` base URL keeps its placeholder: the value is only
    // known once Docker has allocated it, so substituting 0 here would send
    // every request to port 0. Its `used` bookkeeping is already settled by
    // the stricter both-sides check above.
    if backend.uses_auto_port() {
        used = true;
    } else {
        let (base_url, hit) = substitute_port(&backend.base_url, port);
        backend.base_url = base_url;
        used |= hit;
    }

    if let Some(docker) = &mut backend.docker {
        for template in std::iter::once(&mut docker.image)
            .chain(&mut docker.options)
            .chain(&mut docker.args)
        {
            let (resolved, hit) = substitute_port(template, port);
            *template = resolved;
            used |= hit;
        }
    }

    if !used {
        return Err(crate::Error::Config(format!(
            "{}: backend \"{}\": declares \"port\" but never references \
             {{{{ {BACKEND_PLACEHOLDER} }}}}, so the value would be ignored",
            source.display(),
            backend.id
        )));
    }

    Ok(())
}

/// An AI backend configured in `backends/*.toml`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Backend {
    pub id: String,
    pub base_url: String,
    /// TOML field `type` (a reserved Rust word), e.g. `"openai-compatible"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub operations: HashMap<String, Operation>,
    /// Optional: the TCP port this backend listens on, substituted for
    /// `{{ backend.port }}` in `base_url` and in the `[docker]` lists.
    ///
    /// Exists so the port is declared ONCE. Written twice — in `-p` and in
    /// `base_url` — a divergence is invisible to `npu doctor` (its probe
    /// reaches whatever answers on the `base_url` port, possibly another
    /// backend) and only surfaces as exit `3` at execution.
    #[serde(default)]
    pub port: Option<Port>,
    /// Optional: how `npu serve` starts this backend's runtime.
    #[serde(default)]
    pub docker: Option<Docker>,
    /// Optional: overrides `backend::REQUEST_TIMEOUT` for this backend.
    #[serde(default)]
    pub timeouts: Option<Timeouts>,
}

impl Backend {
    /// Does this backend let Docker allocate its port?
    ///
    /// Its `base_url` then still carries `{{ backend.port }}` after loading,
    /// and only `builtin::resolve_base_url` can complete it.
    #[must_use]
    pub fn uses_auto_port(&self) -> bool {
        matches!(&self.port, Some(Port::Keyword(keyword)) if keyword == PORT_AUTO)
    }
}

/// Optional generation parameters for a model.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Generation {
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

/// A model configured in `models/*.toml`, referencing a backend.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Model {
    pub id: String,
    pub backend: String,
    pub operation: String,
    pub model: String,
    /// Model to retry against when this one fails with `Error::Backend`.
    ///
    /// Exists because an NPU-compiled graph has a static maximum prompt
    /// length: OVMS answers `400 ... Input length exceeds the maximum
    /// allowed length` in milliseconds, which is an exact, cheap signal that
    /// the same prompt belongs on a GPU-served model instead. The retry is
    /// SINGLE HOP: the fallback's own `fallback` is not followed, which is
    /// what makes cycle detection unnecessary.
    pub fallback: Option<String>,
    #[serde(default)]
    pub generation: Generation,
}

/// Only backend type supported so far.
///
/// A backend declaring a different `type` would, absent validation, be
/// silently treated as `openai-compatible` by `backend.rs`: we reject it at
/// load time rather than ignore the value (cf. review L3).
const SUPPORTED_BACKEND_KIND: &str = "openai-compatible";

/// Only HTTP method supported in phase 1: `backend.rs` hardcodes
/// `client.post()`. A different `Operation.method`
/// would therefore be silently ignored without this validation (cf. review
/// L3).
const SUPPORTED_METHOD: &str = "POST";

/// The only `{{ args.<name> }}` placeholder a `[docker]` list may reference:
/// `npu serve` takes a model identifier and nothing else, so `model` is the
/// only value it can substitute. Any other name is a configuration error
/// rather than an empty string silently handed to `docker run`.
const DOCKER_PLACEHOLDER_ARG: &str = "model";

/// Validates one `[docker]` list entry: `{{ input }}` has no meaning here
/// (`npu serve` reads no input) and `{{ args.<name> }}` is restricted to
/// [`DOCKER_PLACEHOLDER_ARG`]. `{{ env.NAME }}` passes: it is resolved at
/// `serve` time, against the environment injected by the caller.
fn validate_docker_template(template: &str, backend_id: &str, source: &Path) -> crate::Result<()> {
    for placeholder in crate::prompt::placeholders(template)? {
        match placeholder {
            crate::prompt::Placeholder::Env(_) => {}
            crate::prompt::Placeholder::Arg(name) if name == DOCKER_PLACEHOLDER_ARG => {}
            crate::prompt::Placeholder::Arg(name) => {
                return Err(crate::Error::Config(format!(
                    "{}: backend \"{backend_id}\": [docker] references unknown argument \
                     \"{name}\" (only \"{DOCKER_PLACEHOLDER_ARG}\" is available here)",
                    source.display()
                )));
            }
            crate::prompt::Placeholder::Input => {
                return Err(crate::Error::Config(format!(
                    "{}: backend \"{backend_id}\": [docker] references {{{{ input }}}}, which \
                     has no meaning for a runtime start",
                    source.display()
                )));
            }
        }
    }
    Ok(())
}

/// Characters Docker accepts in a container name (`[a-zA-Z0-9][a-zA-Z0-9_.-]*`).
/// `npu serve` derives the container name from the backend identifier, which
/// is a free-form TOML string: an identifier outside this set is rejected at
/// load time, naming its file, rather than transformed silently or handed to
/// `docker run` to fail on its own terms.
fn is_valid_container_name(id: &str) -> bool {
    let mut chars = id.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_alphanumeric()
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
}

/// Validates the optional `[docker]` table of a backend: the identifier must
/// be usable as a container name, and every list entry must only reference
/// placeholders `serve` can actually resolve.
fn validate_docker(backend: &Backend, source: &Path) -> crate::Result<()> {
    let Some(docker) = &backend.docker else {
        return Ok(());
    };

    if !is_valid_container_name(&backend.id) {
        return Err(crate::Error::Config(format!(
            "{}: backend \"{}\": declaring [docker] requires an identifier usable as a \
             container name (ASCII letters, digits, \"_\", \".\" and \"-\", starting with a \
             letter or a digit)",
            source.display(),
            backend.id
        )));
    }

    for template in docker
        .options
        .iter()
        .chain(docker.args.iter())
        .chain(std::iter::once(&docker.image))
    {
        validate_docker_template(template, &backend.id, source)?;
    }

    Ok(())
}

/// Validates that a loaded backend only declares properties honored in
/// phase 1: `type = "openai-compatible"` and `method = "POST"` for each of
/// its operations. Any other value is a configuration error detected at
/// load time, not a feature to implement.
///
/// Runs AFTER scope merging (cf. `load_scopes`), on surviving entries only:
/// an invalid backend from a general scope, entirely replaced by a more
/// local scope, must never reach this function (cf. review L3, phase 2).
/// `source` is the path of the file the surviving entry comes from, so that
/// the message names the file the user must actually fix.
fn validate_backend(backend: &Backend, source: &Path) -> crate::Result<()> {
    if backend.kind != SUPPORTED_BACKEND_KIND {
        return Err(crate::Error::Config(format!(
            "{}: backend \"{}\": type \"{}\" not supported (only \"{SUPPORTED_BACKEND_KIND}\" \
             is supported in phase 1)",
            source.display(),
            backend.id,
            backend.kind
        )));
    }

    for (operation_name, operation) in &backend.operations {
        if !operation.method.eq_ignore_ascii_case(SUPPORTED_METHOD) {
            return Err(crate::Error::Config(format!(
                "{}: backend \"{}\", operation \"{operation_name}\": method \"{}\" not \
                 supported (only \"{SUPPORTED_METHOD}\" is supported in phase 1)",
                source.display(),
                backend.id,
                operation.method
            )));
        }
    }

    if let Some(timeouts) = &backend.timeouts
        && timeouts.request_secs == 0
    {
        return Err(crate::Error::Config(format!(
            "{}: backend \"{}\": [timeouts].request_secs must be greater than 0",
            source.display(),
            backend.id
        )));
    }

    validate_docker(backend, source)?;

    Ok(())
}

/// Validates a model's `fallback`: it must name another loaded model.
///
/// Pointing at itself is rejected too — the retry is single hop, so a
/// self-fallback is an infinite intent expressed as a no-op, never what the
/// author meant.
fn validate_fallback(
    model: &Model,
    models: &HashMap<String, (Model, PathBuf)>,
    source: &Path,
) -> crate::Result<()> {
    let Some(fallback) = &model.fallback else {
        return Ok(());
    };

    if fallback == &model.id {
        return Err(crate::Error::Config(format!(
            "{}: model \"{}\" declares itself as its own fallback",
            source.display(),
            model.id
        )));
    }

    if !models.contains_key(fallback) {
        return Err(crate::Error::Config(format!(
            "{}: model \"{}\" declares fallback \"{fallback}\", which is not a known model \
             (available models: {})",
            source.display(),
            model.id,
            crate::error::format_available(models.keys())
        )));
    }

    Ok(())
}

/// Resolved configuration: backends and models indexed by their `id`.
#[derive(Debug, Default)]
pub struct Config {
    pub backends: HashMap<String, Backend>,
    pub models: HashMap<String, Model>,
}

/// Loads and deserializes each `*.toml` file in `dir`, indexed by the `id`
/// key that `key_of` produces from the deserialized value. Each entry is
/// paired with the path of the file it comes from, so that the most local
/// scope can carry its origin through to the semantic validation that runs
/// after the merge (cf. `load_scopes`).
///
/// A missing directory produces an empty `HashMap` (this is not an error).
/// An unreadable file or invalid TOML produces an `Error::Config` that
/// mentions the path of the offending file — this PARSING error remains
/// fatal in every scope, unlike semantic validation: before `key_of` could
/// be called, the entry's identity (and thus the question "is it
/// shadowed?") is not knowable (cf. review L3, phase 2).
///
/// Two files in `dir` declaring the same `id` are a configuration
/// ambiguity, not an intention (cf. review L3): `Error::Config` names the
/// duplicated identifier and both file paths involved. The directory's
/// entries are sorted before reading so that this diagnostic (which file is
/// "the first", which file is "the duplicate") is deterministic rather than
/// dependent on filesystem order.
///
/// This constraint only holds within `dir`: `load_scopes` merges several
/// calls to this function (one per scope), where replacement by `id` is
/// precisely the requested feature, not an ambiguity.
fn load_toml_dir<T, F>(dir: &Path, key_of: F) -> crate::Result<HashMap<String, (T, PathBuf)>>
where
    T: for<'de> Deserialize<'de>,
    F: Fn(&T) -> String,
{
    let mut out: HashMap<String, (T, PathBuf)> = HashMap::new();
    let mut sources: HashMap<String, PathBuf> = HashMap::new();

    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(err) => {
            return Err(crate::Error::Config(format!(
                "cannot read directory {}: {err}",
                dir.display()
            )));
        }
    };

    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| {
            crate::Error::Config(format!(
                "cannot read a directory entry from {}: {err}",
                dir.display()
            ))
        })?;
        let path = entry.path();
        if path.extension().and_then(std::ffi::OsStr::to_str) == Some("toml") {
            paths.push(path);
        }
    }
    paths.sort_unstable();

    for path in paths {
        let contents = std::fs::read_to_string(&path).map_err(|err| {
            crate::Error::Config(format!("cannot read file {}: {err}", path.display()))
        })?;
        let value: T = toml::from_str(&contents).map_err(|err| {
            crate::Error::Config(format!("invalid TOML in {}: {err}", path.display()))
        })?;
        let key = key_of(&value);

        if let Some(previous_path) = sources.get(&key) {
            return Err(crate::Error::Config(format!(
                "identifier \"{key}\" defined multiple times in the same scope: {} and {} \
                 (within a single scope, each identifier must be unique; across scopes, \
                 redefinition is the expected feature)",
                previous_path.display(),
                path.display()
            )));
        }

        sources.insert(key.clone(), path.clone());
        out.insert(key, (value, path));
    }

    Ok(out)
}

/// Loads `<root>/backends/*.toml` and `<root>/models/*.toml`: a simple
/// single scope, in `load_scopes` terms. This is not duplicated into a
/// separate loading path: single-scope and multi-scope loading can
/// therefore never diverge on when semantic validation runs (cf. review L3,
/// phase 2).
///
/// The key of each table is the file's `id` field.
pub fn load(root: &Path) -> crate::Result<Config> {
    load_scopes(&[root.to_path_buf()])
}

/// Loads and merges several configuration scopes.
///
/// `roots` must be ordered from most general to most local — this is the
/// order `scope::roots()` produces. Each root is loaded with
/// `load_toml_dir`, then merged in the order received: for both `backends`
/// and `models`, an entry from a more local scope entirely replaces the one
/// with the same `id` coming from a more general scope (`HashMap::extend`
/// applied in root order — no field-by-field merge; a field missing from a
/// local redefinition is not inherited from the general scope).
///
/// Semantic validation (`validate_backend`) runs AFTER this merge, and only
/// on surviving entries: an invalid backend from a general scope, entirely
/// shadowed by a more local scope, must never make loading fail (cf. review
/// L3, phase 2 — unlike a TOML PARSING error, which remains fatal in every
/// scope since the identity of an unreadable file is not knowable, and thus
/// neither is whether it is shadowed: cf. `load_toml_dir`).
///
/// A root in `roots` that does not exist on disk is not an error:
/// `load_toml_dir` already returns empty tables in that case (a `NotFound`
/// directory is treated as empty), so nothing special is needed here. An
/// empty `roots` list produces an empty `Config`.
pub fn load_scopes(roots: &[PathBuf]) -> crate::Result<Config> {
    let mut backends: HashMap<String, (Backend, PathBuf)> = HashMap::new();
    let mut models: HashMap<String, (Model, PathBuf)> = HashMap::new();

    for root in roots {
        let scope_backends = load_toml_dir::<Backend, _>(&root.join("backends"), |b| b.id.clone())?;
        backends.extend(scope_backends);
        let scope_models = load_toml_dir::<Model, _>(&root.join("models"), |m| m.id.clone())?;
        models.extend(scope_models);
    }

    // Before `validate_backend`: substituting `{{ backend.port }}` here means
    // everything downstream — the docker template whitelist included — only
    // ever sees a resolved value.
    for (backend, source) in backends.values_mut() {
        resolve_port(backend, source)?;
    }

    for (backend, source) in backends.values() {
        validate_backend(backend, source)?;
    }

    // A `fallback` is a COLD path: left unchecked it would only fail the day
    // the primary model fails, i.e. exactly when the recovery is needed.
    // Checked here, after the merge, so that a fallback declared in a general
    // scope and satisfied by a model from a more local scope stays valid.
    for (model, source) in models.values() {
        validate_fallback(model, &models, source)?;
    }

    let backends = backends.into_iter().map(|(id, (b, _))| (id, b)).collect();
    let models = models.into_iter().map(|(id, (m, _))| (id, m)).collect();

    Ok(Config { backends, models })
}

impl Config {
    /// Resolves a model identifier to the corresponding `(Model, Backend)` pair.
    ///
    /// Returns `Error::Config` if the model is unknown, or if its backend does
    /// not exist; in both cases the message lists the available identifiers.
    pub fn resolve(&self, model_id: &str) -> crate::Result<(&Model, &Backend)> {
        let Some(model) = self.models.get(model_id) else {
            return Err(crate::Error::Config(format!(
                "unknown model: \"{model_id}\" (available models: {})",
                crate::error::format_available(self.models.keys())
            )));
        };

        let Some(backend) = self.backends.get(&model.backend) else {
            return Err(crate::Error::Config(format!(
                "unknown backend: \"{}\" (referenced by model \"{model_id}\", available backends: {})",
                model.backend,
                crate::error::format_available(self.backends.keys())
            )));
        };

        Ok((model, backend))
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)] // tolerated in tests (cf. Cargo.toml [lints.clippy]).
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Creates a unique fixture directory under `target/`, so as not to
    /// pollute the repo or collide between tests run in parallel.
    fn fixture_dir(name: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-fixtures")
            .join(format!("config-{name}-{n}"));
        std::fs::create_dir_all(&dir).expect("fixture directory creation");
        dir
    }

    fn write(dir: &Path, rel: &str, contents: &str) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("parent directory creation");
        }
        std::fs::write(path, contents).expect("fixture write");
    }

    /// Writes a backend declaring `port = <port_value>` (raw TOML), whose
    /// `base_url` and `[docker]` both read `{{ backend.port }}`.
    fn write_port_backend(root: &Path, port_value: &str) {
        write(
            root,
            "backends/b.toml",
            &format!(
                r#"
                id = "b"
                base_url = "http://127.0.0.1:{{{{ backend.port }}}}"
                type = "openai-compatible"
                port = {port_value}

                [operations.chat]
                method = "POST"
                path = "/v1/chat/completions"

                [docker]
                image = "img"
                options = ["-p", "{{{{ backend.port }}}}:8000"]
                "#
            ),
        );
    }

    #[test]
    fn port_number_is_substituted_in_base_url_and_docker() {
        let root = fixture_dir("port-fixed");
        write_port_backend(&root, "8001");

        let config = load(&root).expect("a declared port must load");
        let backend = config.backends.get("b").expect("backend b");

        assert_eq!(backend.base_url, "http://127.0.0.1:8001");
        assert_eq!(
            backend.docker.as_ref().expect("docker table").options,
            vec!["-p".to_string(), "8001:8000".to_string()]
        );
    }

    #[test]
    fn port_auto_leaves_base_url_templated_and_asks_docker_for_an_ephemeral_port() {
        let root = fixture_dir("port-auto");
        write_port_backend(&root, "\"auto\"");

        let config = load(&root).expect("port = \"auto\" must load");
        let backend = config.backends.get("b").expect("backend b");

        assert!(backend.uses_auto_port());
        // Still templated: the value does not exist until Docker allocates
        // it, and `builtin::resolve_base_url` reads it back.
        assert_eq!(backend.base_url, "http://127.0.0.1:{{ backend.port }}");
        // `-p 0:8000` is how Docker is asked to allocate a free one.
        assert_eq!(
            backend.docker.as_ref().expect("docker table").options[1],
            format!("{DOCKER_EPHEMERAL_PORT}:8000")
        );
    }

    #[test]
    fn port_auto_without_a_docker_table_is_rejected() {
        let root = fixture_dir("port-auto-no-docker");
        write(
            &root,
            "backends/b.toml",
            r#"
            id = "b"
            base_url = "http://127.0.0.1:{{ backend.port }}"
            type = "openai-compatible"
            port = "auto"

            [operations.chat]
            method = "POST"
            path = "/v1/chat/completions"
            "#,
        );

        let err = load(&root).expect_err("auto without [docker] must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("b.toml"), "got: {err}");
    }

    #[test]
    fn port_auto_whose_base_url_ignores_the_placeholder_is_rejected() {
        let root = fixture_dir("port-auto-fixed-url");
        write(
            &root,
            "backends/b.toml",
            r#"
            id = "b"
            base_url = "http://127.0.0.1:8001"
            type = "openai-compatible"
            port = "auto"

            [operations.chat]
            method = "POST"
            path = "/v1/chat/completions"

            [docker]
            image = "img"
            options = ["-p", "{{ backend.port }}:8000"]
            "#,
        );

        let err = load(&root).expect_err("an auto port no base_url reads must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("b.toml"), "got: {err}");
    }

    /// Symmetric to the `base_url` case: with no `-p {{ backend.port }}`
    /// the container publishes nothing, so `docker port` has nothing to
    /// report and every command fails on advice that could never work.
    #[test]
    fn port_auto_whose_docker_table_ignores_the_placeholder_is_rejected() {
        let root = fixture_dir("port-auto-unpublished");
        write(
            &root,
            "backends/b.toml",
            r#"
            id = "b"
            base_url = "http://127.0.0.1:{{ backend.port }}"
            type = "openai-compatible"
            port = "auto"

            [operations.chat]
            method = "POST"
            path = "/v1/chat/completions"

            [docker]
            image = "img"
            options = ["--rm"]
            "#,
        );

        let err = load(&root).expect_err("an auto port nothing publishes must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("b.toml"), "got: {err}");
    }

    #[test]
    fn port_placeholder_without_the_key_is_rejected() {
        let root = fixture_dir("port-missing-key");
        write(
            &root,
            "backends/b.toml",
            r#"
            id = "b"
            base_url = "http://127.0.0.1:{{ backend.port }}"
            type = "openai-compatible"

            [operations.chat]
            method = "POST"
            path = "/v1/chat/completions"
            "#,
        );

        let err = load(&root).expect_err("an unresolvable placeholder must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("b.toml"), "got: {err}");
    }

    #[test]
    fn port_key_never_referenced_is_rejected() {
        let root = fixture_dir("port-unused");
        write(
            &root,
            "backends/b.toml",
            r#"
            id = "b"
            base_url = "http://127.0.0.1:8001"
            type = "openai-compatible"
            port = 8001

            [operations.chat]
            method = "POST"
            path = "/v1/chat/completions"
            "#,
        );

        let err = load(&root).expect_err("a port nothing reads must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("b.toml"), "got: {err}");
    }

    #[test]
    fn port_keyword_other_than_auto_is_rejected() {
        let root = fixture_dir("port-bad-keyword");
        write_port_backend(&root, "\"random\"");

        let err = load(&root).expect_err("an unknown port keyword must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
        let message = err.to_string();
        assert!(message.contains("b.toml"), "got: {message}");
        assert!(message.contains("random"), "got: {message}");
    }

    #[test]
    fn port_zero_is_rejected() {
        let root = fixture_dir("port-zero");
        write_port_backend(&root, "0");

        let err = load(&root).expect_err("port 0 must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("b.toml"), "got: {err}");
    }

    #[test]
    fn substitute_port_leaves_other_placeholders_alone() {
        let (out, hit) = substitute_port("{{ args.model }}:{{backend.port}}", 8001);
        assert_eq!(out, "{{ args.model }}:8001");
        assert!(hit);

        let (out, hit) = substitute_port("{{ args.model }}", 8001);
        assert_eq!(out, "{{ args.model }}");
        assert!(!hit);
    }

    /// The scan skips a non-matching placeholder by advancing past its `{{`
    /// only, so its body is re-scanned. This is the case that would expose a
    /// mis-paired `}}`: a `{{ env.NAME }}` BEFORE the port, in the same
    /// string — the exact shape of a volume option in a real `[docker]`
    /// table.
    #[test]
    fn substitute_port_after_another_placeholder_in_the_same_string() {
        let (out, hit) = substitute_port("{{ env.HOME }}/m:{{ backend.port }}:8000", 8001);
        assert_eq!(out, "{{ env.HOME }}/m:8001:8000");
        assert!(hit);

        // And the detection used by `resolve_port` to reject a placeholder
        // with no `port` key must see it in that same position.
        assert!(substitute_port("{{ env.HOME }}:{{ backend.port }}", 0).1);
    }

    /// An unclosed `{{` must not swallow the rest, matching `prompt.rs`'s own
    /// rule that an unclosed placeholder is left alone.
    #[test]
    fn substitute_port_leaves_an_unclosed_brace_alone() {
        let (out, hit) = substitute_port("{{ backend.port", 8001);
        assert_eq!(out, "{{ backend.port");
        assert!(!hit);
    }

    /// The three `fallback` tests share this backend: only the models vary.
    const FALLBACK_BACKEND: &str = r#"
        id = "b"
        base_url = "http://127.0.0.1:8000"
        type = "openai-compatible"

        [operations.chat]
        method = "POST"
        path = "/v1/chat/completions"
    "#;

    #[test]
    fn fallback_pointing_at_a_known_model_loads() {
        let root = fixture_dir("fallback-ok");
        write(&root, "backends/b.toml", FALLBACK_BACKEND);
        write(
            &root,
            "models/npu.toml",
            r#"
            id = "npu"
            backend = "b"
            operation = "chat"
            model = "m-npu"
            fallback = "gpu"
            "#,
        );
        write(
            &root,
            "models/gpu.toml",
            r#"
            id = "gpu"
            backend = "b"
            operation = "chat"
            model = "m-gpu"
            "#,
        );

        let config = load(&root).expect("a fallback naming a loaded model must load");
        assert_eq!(
            config.models.get("npu").and_then(|m| m.fallback.as_deref()),
            Some("gpu")
        );
    }

    #[test]
    fn fallback_pointing_at_an_unknown_model_is_rejected_naming_both() {
        let root = fixture_dir("fallback-unknown");
        write(&root, "backends/b.toml", FALLBACK_BACKEND);
        write(
            &root,
            "models/npu.toml",
            r#"
            id = "npu"
            backend = "b"
            operation = "chat"
            model = "m-npu"
            fallback = "nope"
            "#,
        );

        let err = load(&root).expect_err("an unknown fallback must be rejected at load time");
        assert!(matches!(err, crate::Error::Config(_)));
        let message = err.to_string();
        assert!(message.contains("npu.toml"), "got: {message}");
        assert!(message.contains("nope"), "got: {message}");
    }

    #[test]
    fn fallback_pointing_at_itself_is_rejected() {
        let root = fixture_dir("fallback-self");
        write(&root, "backends/b.toml", FALLBACK_BACKEND);
        write(
            &root,
            "models/npu.toml",
            r#"
            id = "npu"
            backend = "b"
            operation = "chat"
            model = "m-npu"
            fallback = "npu"
            "#,
        );

        let err = load(&root).expect_err("a self-fallback must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("npu.toml"), "got: {err}");
    }

    #[test]
    fn resolve_returns_model_and_backend() {
        let root = fixture_dir("resolve-ok");
        write(
            &root,
            "backends/openai.toml",
            r#"
            id = "openai"
            base_url = "https://api.openai.com"
            type = "openai-compatible"

            [operations.chat]
            method = "POST"
            path = "/v1/chat/completions"
            "#,
        );
        write(
            &root,
            "models/gpt.toml",
            r#"
            id = "gpt"
            backend = "openai"
            operation = "chat"
            model = "gpt-4o"
            "#,
        );

        let config = load(&root).expect("config loading");
        let (model, backend) = config.resolve("gpt").expect("model resolution");
        assert_eq!(model.id, "gpt");
        assert_eq!(backend.id, "openai");
        assert_eq!(backend.kind, "openai-compatible");
    }

    #[test]
    fn resolve_unknown_model_lists_available_ids() {
        let root = fixture_dir("resolve-unknown-model");
        write(
            &root,
            "backends/openai.toml",
            r#"
            id = "openai"
            base_url = "https://api.openai.com"
            type = "openai-compatible"

            [operations.chat]
            method = "POST"
            path = "/v1/chat/completions"
            "#,
        );
        write(
            &root,
            "models/gpt.toml",
            r#"
            id = "gpt"
            backend = "openai"
            operation = "chat"
            model = "gpt-4o"
            "#,
        );

        let config = load(&root).expect("config loading");
        let err = config.resolve("unknown").expect_err("must fail");
        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("gpt"));
    }

    #[test]
    fn resolve_missing_backend_lists_available_ids() {
        let root = fixture_dir("resolve-missing-backend");
        write(
            &root,
            "models/gpt.toml",
            r#"
            id = "gpt"
            backend = "absent"
            operation = "chat"
            model = "gpt-4o"
            "#,
        );

        let config = load(&root).expect("config loading");
        let err = config.resolve("gpt").expect_err("must fail");
        assert!(matches!(err, crate::Error::Config(_)));
        let msg = err.to_string();
        assert!(msg.contains("absent"));
    }

    #[test]
    fn load_missing_directories_yields_empty_config() {
        let root = fixture_dir("load-missing-dirs");
        let config = load(&root).expect("a missing directory is not an error");
        assert!(config.backends.is_empty());
        assert!(config.models.is_empty());
    }

    #[test]
    fn load_invalid_toml_reports_file_path() {
        let root = fixture_dir("load-invalid-toml");
        write(&root, "backends/broken.toml", "not = [valid");

        let err = load(&root).expect_err("invalid TOML must fail");
        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("broken.toml"));
    }

    #[test]
    fn load_rejects_backend_operation_with_non_post_method() {
        let root = fixture_dir("reject-non-post-method");
        write(
            &root,
            "backends/ovms.toml",
            r#"
            id = "ovms"
            base_url = "http://127.0.0.1:8000"
            type = "openai-compatible"

            [operations.chat]
            method = "GET"
            path = "/v3/chat/completions"
            "#,
        );

        let err = load(&root).expect_err("a non-POST method must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
        let msg = err.to_string();
        assert!(msg.contains("ovms"));
    }

    #[test]
    fn load_accepts_a_backend_declaring_docker() {
        let root = fixture_dir("accept-docker-table");
        write(
            &root,
            "backends/ovms.toml",
            r#"
            id = "ovms"
            base_url = "http://127.0.0.1:8000"
            type = "openai-compatible"

            [operations.chat]
            method = "POST"
            path = "/v3/chat/completions"

            [docker]
            image = "openvino/model_server:latest"
            options = ["-p", "8000:8000", "-v", "{{ env.HOME }}/models:/models:rw"]
            args = ["--source_model", "{{ args.model }}", "--rest_port", "8000"]
            "#,
        );

        let config = load(&root).expect("a valid [docker] table must load");
        let docker = config.backends["ovms"]
            .docker
            .as_ref()
            .expect("the [docker] table must be kept");
        assert_eq!(docker.image, "openvino/model_server:latest");
        assert_eq!(docker.options.len(), 4);
        assert_eq!(docker.args.len(), 4);
    }

    #[test]
    fn load_accepts_a_docker_table_reduced_to_its_image() {
        let root = fixture_dir("accept-docker-image-only");
        write(
            &root,
            "backends/ovms.toml",
            r#"
            id = "ovms"
            base_url = "http://127.0.0.1:8000"
            type = "openai-compatible"

            [operations.chat]
            method = "POST"
            path = "/v3/chat/completions"

            [docker]
            image = "openvino/model_server:latest"
            "#,
        );

        let config = load(&root).expect("[docker] reduced to its image must load");
        let docker = config.backends["ovms"]
            .docker
            .as_ref()
            .expect("the [docker] table must be kept");
        assert!(docker.options.is_empty());
        assert!(docker.args.is_empty());
    }

    #[test]
    fn load_rejects_docker_referencing_an_unknown_argument() {
        let root = fixture_dir("reject-docker-unknown-arg");
        write(
            &root,
            "backends/ovms.toml",
            r#"
            id = "ovms"
            base_url = "http://127.0.0.1:8000"
            type = "openai-compatible"

            [operations.chat]
            method = "POST"
            path = "/v3/chat/completions"

            [docker]
            image = "openvino/model_server:latest"
            args = ["--source_model", "{{ args.modle }}"]
            "#,
        );

        let err = load(&root).expect_err("a misspelled argument must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
        let msg = err.to_string();
        assert!(msg.contains("ovms.toml"), "the message must name the file");
        assert!(
            msg.contains("modle"),
            "the message must name the placeholder"
        );
    }

    #[test]
    fn load_rejects_docker_referencing_the_input() {
        let root = fixture_dir("reject-docker-input");
        write(
            &root,
            "backends/ovms.toml",
            r#"
            id = "ovms"
            base_url = "http://127.0.0.1:8000"
            type = "openai-compatible"

            [operations.chat]
            method = "POST"
            path = "/v3/chat/completions"

            [docker]
            image = "openvino/model_server:latest"
            options = ["{{ input }}"]
            "#,
        );

        let err = load(&root).expect_err("{{ input }} has no meaning in [docker]");
        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("ovms.toml"));
    }

    #[test]
    fn load_rejects_docker_on_a_backend_whose_id_is_not_a_container_name() {
        let root = fixture_dir("reject-docker-non-ascii-id");
        write(
            &root,
            "backends/cafe.toml",
            r#"
            id = "café"
            base_url = "http://127.0.0.1:8000"
            type = "openai-compatible"

            [operations.chat]
            method = "POST"
            path = "/v3/chat/completions"

            [docker]
            image = "openvino/model_server:latest"
            "#,
        );

        let err = load(&root).expect_err("a non-ASCII id cannot name a container");
        assert!(matches!(err, crate::Error::Config(_)));
        let msg = err.to_string();
        assert!(msg.contains("cafe.toml"), "the message must name the file");
        assert!(msg.contains("café"), "the message must name the backend");
    }

    #[test]
    fn load_accepts_a_non_ascii_backend_id_without_docker() {
        let root = fixture_dir("accept-non-ascii-id-without-docker");
        write(
            &root,
            "backends/cafe.toml",
            r#"
            id = "café"
            base_url = "http://127.0.0.1:8000"
            type = "openai-compatible"

            [operations.chat]
            method = "POST"
            path = "/v3/chat/completions"
            "#,
        );

        let config = load(&root).expect("the container-name constraint only applies to [docker]");
        assert!(config.backends.contains_key("café"));
    }

    #[test]
    fn load_rejects_backend_with_unknown_kind() {
        let root = fixture_dir("reject-unknown-kind");
        write(
            &root,
            "backends/ollama.toml",
            r#"
            id = "ollama"
            base_url = "http://127.0.0.1:11434"
            type = "ollama"

            [operations.chat]
            method = "POST"
            path = "/api/chat"
            "#,
        );

        let err = load(&root).expect_err("an unknown backend type must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
        let msg = err.to_string();
        assert!(msg.contains("ollama"));
    }

    #[test]
    fn load_rejects_backend_with_unknown_toml_key() {
        let root = fixture_dir("reject-unknown-backend-key");
        write(
            &root,
            "backends/ovms.toml",
            r#"
            id = "ovms"
            base_url = "http://127.0.0.1:8000"
            type = "openai-compatible"
            retries = 3

            [operations.chat]
            method = "POST"
            path = "/v3/chat/completions"
            "#,
        );

        let err = load(&root).expect_err("an unknown TOML key on a backend must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn load_rejects_timeouts_with_unknown_toml_key() {
        let root = fixture_dir("reject-unknown-timeouts-key");
        write(
            &root,
            "backends/ovms.toml",
            r#"
            id = "ovms"
            base_url = "http://127.0.0.1:8000"
            type = "openai-compatible"

            [timeouts]
            connect_secs = 5

            [operations.chat]
            method = "POST"
            path = "/v3/chat/completions"
            "#,
        );

        let err = load(&root).expect_err("an unknown key inside [timeouts] must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("ovms.toml"));
    }

    #[test]
    fn load_rejects_zero_request_secs() {
        let root = fixture_dir("reject-zero-request-secs");
        write(
            &root,
            "backends/ovms.toml",
            r#"
            id = "ovms"
            base_url = "http://127.0.0.1:8000"
            type = "openai-compatible"

            [timeouts]
            request_secs = 0

            [operations.chat]
            method = "POST"
            path = "/v3/chat/completions"
            "#,
        );

        let err = load(&root).expect_err("a zero request_secs must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
        let msg = err.to_string();
        assert!(msg.contains("ovms.toml"));
        assert!(msg.contains("ovms"));
    }

    #[test]
    fn load_accepts_valid_timeouts() {
        let root = fixture_dir("accept-valid-timeouts");
        write(
            &root,
            "backends/ovms.toml",
            r#"
            id = "ovms"
            base_url = "http://127.0.0.1:8000"
            type = "openai-compatible"

            [timeouts]
            request_secs = 120

            [operations.chat]
            method = "POST"
            path = "/v3/chat/completions"
            "#,
        );

        let config = load(&root).expect("a valid [timeouts] table must load");
        let backend = &config.backends["ovms"];
        let timeouts = backend
            .timeouts
            .as_ref()
            .expect("[timeouts] must be present");
        assert_eq!(timeouts.request_secs, 120);
    }

    #[test]
    fn load_rejects_model_with_unknown_toml_key() {
        let root = fixture_dir("reject-unknown-model-key");
        write(
            &root,
            "models/gpt.toml",
            r#"
            id = "gpt"
            backend = "openai"
            operation = "chat"
            model = "gpt-4o"

            [generation]
            temperature = 0.0
            max_tokes = 512
            "#,
        );

        let err = load(&root).expect_err("an unknown TOML key on a model must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn real_npu_fixture_still_parses() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".npu");
        let config = load(&root).expect("the real .npu/ fixture must always load");

        let backend = config
            .backends
            .get("ovms")
            .expect("the ovms backend must be present");
        assert_eq!(backend.kind, "openai-compatible");
        assert!(backend.operations.contains_key("chat"));

        // No model assertion: the project scope declares backends and
        // commands only. Its models live in the user scope since feb76a7 —
        // they are tied to this machine's local `optimum-cli` exports, which
        // nothing in the repository can provide.
    }

    #[test]
    fn load_rejects_duplicate_id_within_same_scope_naming_both_files() {
        let root = fixture_dir("duplicate-id-same-scope");
        write(
            &root,
            "models/a.toml",
            r#"
            id = "qwen-fast"
            backend = "ovms"
            operation = "chat"
            model = "qwen-2.5-1.5b"
            "#,
        );
        write(
            &root,
            "models/b.toml",
            r#"
            id = "qwen-fast"
            backend = "ovms"
            operation = "chat"
            model = "qwen-2.5-3b"
            "#,
        );

        let err = load(&root).expect_err("a duplicate id within the same scope must fail");
        assert!(matches!(err, crate::Error::Config(_)));
        let msg = err.to_string();
        assert!(msg.contains("qwen-fast"));
        assert!(msg.contains("a.toml"));
        assert!(msg.contains("b.toml"));
    }

    #[test]
    fn load_scopes_local_backend_replaces_general_one_entirely() {
        let general = fixture_dir("scopes-backend-general");
        write(
            &general,
            "backends/ovms.toml",
            r#"
            id = "ovms"
            base_url = "http://general:8000"
            type = "openai-compatible"

            [operations.chat]
            method = "POST"
            path = "/v3/chat/completions"

            [operations.embeddings]
            method = "POST"
            path = "/v3/embeddings"
            "#,
        );

        let local = fixture_dir("scopes-backend-local");
        write(
            &local,
            "backends/ovms.toml",
            r#"
            id = "ovms"
            base_url = "http://local:8000"
            type = "openai-compatible"

            [operations.chat]
            method = "POST"
            path = "/v3/chat/completions"
            "#,
        );

        let config = load_scopes(&[general, local]).expect("scope merging must succeed");
        let backend = config
            .backends
            .get("ovms")
            .expect("the ovms backend must be present");
        assert_eq!(backend.base_url, "http://local:8000");
        // The field absent from the local redefinition (embeddings) must NOT
        // be inherited from the general scope: full replacement, not a
        // field-by-field merge.
        assert!(!backend.operations.contains_key("embeddings"));
        assert!(backend.operations.contains_key("chat"));
    }

    #[test]
    fn load_scopes_local_model_resolves_against_general_scope_backend() {
        let general = fixture_dir("scopes-model-general");
        write(
            &general,
            "backends/ovms.toml",
            r#"
            id = "ovms"
            base_url = "http://127.0.0.1:8000"
            type = "openai-compatible"

            [operations.chat]
            method = "POST"
            path = "/v3/chat/completions"
            "#,
        );

        let local = fixture_dir("scopes-model-local");
        write(
            &local,
            "models/qwen-fast.toml",
            r#"
            id = "qwen-fast"
            backend = "ovms"
            operation = "chat"
            model = "qwen-2.5-1.5b"
            "#,
        );

        let config = load_scopes(&[general, local]).expect("scope merging must succeed");
        let (model, backend) = config
            .resolve("qwen-fast")
            .expect("the local model must resolve the general backend");
        assert_eq!(model.id, "qwen-fast");
        assert_eq!(backend.id, "ovms");
    }

    #[test]
    fn load_scopes_nonexistent_root_is_ignored() {
        let existing = fixture_dir("scopes-nonexistent-existing");
        write(
            &existing,
            "models/gpt.toml",
            r#"
            id = "gpt"
            backend = "openai"
            operation = "chat"
            model = "gpt-4o"
            "#,
        );
        let missing = existing.join("does-not-exist");

        let config =
            load_scopes(&[missing, existing]).expect("a missing root must not make the merge fail");
        assert!(config.models.contains_key("gpt"));
    }

    #[test]
    fn load_scopes_empty_roots_yields_empty_config() {
        let config = load_scopes(&[]).expect("an empty root list must succeed");
        assert!(config.backends.is_empty());
        assert!(config.models.is_empty());
    }

    #[test]
    fn load_scopes_precedence_is_general_to_local() {
        let etc = fixture_dir("scopes-precedence-etc");
        write(
            &etc,
            "models/qwen-fast.toml",
            r#"
            id = "qwen-fast"
            backend = "ovms"
            operation = "chat"
            model = "etc-model"
            "#,
        );

        let xdg = fixture_dir("scopes-precedence-xdg");
        write(
            &xdg,
            "models/qwen-fast.toml",
            r#"
            id = "qwen-fast"
            backend = "ovms"
            operation = "chat"
            model = "xdg-model"
            "#,
        );

        let cwd = fixture_dir("scopes-precedence-cwd");
        write(
            &cwd,
            "models/qwen-fast.toml",
            r#"
            id = "qwen-fast"
            backend = "ovms"
            operation = "chat"
            model = "cwd-model"
            "#,
        );

        let config =
            load_scopes(&[etc.clone(), xdg.clone(), cwd.clone()]).expect("the merge must succeed");
        assert_eq!(
            config.models.get("qwen-fast").expect("qwen-fast").model,
            "cwd-model",
            "the most local root (cwd, at the end of the list) must win"
        );

        // A partial order (just etc then xdg, without cwd) must also
        // respect general -> local.
        let config2 = load_scopes(&[etc, xdg]).expect("the two-root merge must succeed");
        assert_eq!(
            config2.models.get("qwen-fast").expect("qwen-fast").model,
            "xdg-model"
        );
    }

    #[test]
    fn load_scopes_invalid_backend_fully_masked_by_local_scope_resolves_successfully() {
        // Review L3 (phase 2): an invalid backend from a general scope,
        // entirely replaced by a valid local scope, must never reach
        // semantic validation.
        let general = fixture_dir("scopes-invalid-backend-masked-general");
        write(
            &general,
            "backends/ovms.toml",
            r#"
            id = "ovms"
            base_url = "http://x"
            type = "ollama"

            [operations.chat]
            method = "POST"
            path = "/x"
            "#,
        );

        let local = fixture_dir("scopes-invalid-backend-masked-local");
        write(
            &local,
            "backends/ovms.toml",
            r#"
            id = "ovms"
            base_url = "http://local:8000"
            type = "openai-compatible"

            [operations.chat]
            method = "POST"
            path = "/v3/chat/completions"
            "#,
        );

        let config = load_scopes(&[general, local])
            .expect("the shadowed invalid backend must not prevent resolution");
        let backend = config
            .backends
            .get("ovms")
            .expect("the ovms backend (local version) must be present");
        assert_eq!(backend.kind, "openai-compatible");
        assert_eq!(backend.base_url, "http://local:8000");
    }

    #[test]
    fn load_scopes_invalid_backend_not_masked_still_fails_and_names_its_file() {
        // The reverse case, the easiest to break by fixing the first one: an
        // invalid backend present ONLY in a general scope must always fail,
        // and the message must name its file (not just its id), since
        // validation now runs after the merge on an entry that carries its
        // origin.
        let general = fixture_dir("scopes-invalid-backend-unmasked-general");
        write(
            &general,
            "backends/ovms.toml",
            r#"
            id = "ovms"
            base_url = "http://x"
            type = "ollama"

            [operations.chat]
            method = "POST"
            path = "/x"
            "#,
        );

        let err =
            load_scopes(&[general]).expect_err("an unshadowed invalid backend must always fail");
        assert!(matches!(err, crate::Error::Config(_)));
        let msg = err.to_string();
        assert!(
            msg.contains("ovms.toml"),
            "the message must name the offending file, got: {msg}"
        );
    }

    #[test]
    fn load_scopes_toml_parse_error_in_general_scope_remains_fatal_even_when_masked() {
        // Unlike semantic validation, a PARSING error remains fatal in
        // every scope: an unreadable file has no knowable identity, so we
        // cannot know whether it is shadowed (cf. review L3, phase 2). This
        // is not a bug.
        let general = fixture_dir("scopes-parse-error-masked-general");
        write(&general, "backends/ovms.toml", "not = [valid");

        let local = fixture_dir("scopes-parse-error-masked-local");
        write(
            &local,
            "backends/ovms.toml",
            r#"
            id = "ovms"
            base_url = "http://local:8000"
            type = "openai-compatible"

            [operations.chat]
            method = "POST"
            path = "/v3/chat/completions"
            "#,
        );

        let err = load_scopes(&[general, local]).expect_err(
            "a TOML parsing error in a general scope must remain fatal even when a valid id \
             exists locally",
        );
        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("ovms.toml"));
    }
}

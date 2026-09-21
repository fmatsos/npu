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

    for (backend, source) in backends.values() {
        validate_backend(backend, source)?;
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

            [timeouts]
            connect = "500ms"

            [operations.chat]
            method = "POST"
            path = "/v3/chat/completions"
            "#,
        );

        let err = load(&root).expect_err("an unknown TOML key on a backend must be rejected");
        assert!(matches!(err, crate::Error::Config(_)));
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

        let model = config
            .models
            .get("qwen-fast")
            .expect("the qwen-fast model must be present");
        assert_eq!(model.generation.temperature, Some(0.0));
        assert_eq!(model.generation.max_tokens, Some(512));
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

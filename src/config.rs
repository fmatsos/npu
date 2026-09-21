//! Chargement de la configuration : backends et modèles.
//!
//! Phase 1 : un seul scope (`./.npu`), pas de fusion de scopes (cf. IMPLEMENTATION.md §2).

use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

/// Une opération HTTP exposée par un backend (ex. `chat`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Operation {
    pub method: String,
    pub path: String,
}

/// Un backend IA configuré dans `backends/*.toml`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Backend {
    pub id: String,
    pub base_url: String,
    /// Champ TOML `type` (mot réservé Rust), ex. `"openai-compatible"`.
    #[serde(rename = "type")]
    pub kind: String,
    pub operations: HashMap<String, Operation>,
}

/// Paramètres de génération optionnels d'un modèle.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Generation {
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

/// Un modèle configuré dans `models/*.toml`, référençant un backend.
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

/// Seul type de backend supporté en phase 1 (cf. npu-cli-spec.md §7).
///
/// Un backend déclarant un autre `type` serait, faute de validation, traité
/// silencieusement comme `openai-compatible` par `backend.rs` : on rejette au
/// chargement plutôt que d'ignorer la valeur (cf. revue L3).
const SUPPORTED_BACKEND_KIND: &str = "openai-compatible";

/// Seule méthode HTTP supportée en phase 1 : `backend.rs` code `client.post()`
/// en dur (cf. npu-cli-spec.md §7). Une `Operation.method` différente serait
/// donc silencieusement ignorée sans cette validation (cf. revue L3).
const SUPPORTED_METHOD: &str = "POST";

/// Valide qu'un backend chargé ne déclare que des propriétés honorées en
/// phase 1 : `type = "openai-compatible"` et `method = "POST"` pour chacune
/// de ses opérations. Toute autre valeur est une erreur de configuration
/// détectée au chargement, pas une fonctionnalité à implémenter.
fn validate_backend(backend: &Backend) -> crate::Result<()> {
    if backend.kind != SUPPORTED_BACKEND_KIND {
        return Err(crate::Error::Config(format!(
            "backend « {} » : type « {} » non supporté (seul « {SUPPORTED_BACKEND_KIND} » est \
             supporté en phase 1)",
            backend.id, backend.kind
        )));
    }

    for (operation_name, operation) in &backend.operations {
        if !operation.method.eq_ignore_ascii_case(SUPPORTED_METHOD) {
            return Err(crate::Error::Config(format!(
                "backend « {} », opération « {operation_name} » : méthode « {} » non supportée \
                 (seule « {SUPPORTED_METHOD} » est supportée en phase 1)",
                backend.id, operation.method
            )));
        }
    }

    Ok(())
}

/// Configuration résolue : backends et modèles indexés par leur `id`.
#[derive(Debug, Default)]
pub struct Config {
    pub backends: HashMap<String, Backend>,
    pub models: HashMap<String, Model>,
}

/// Charge et désérialise chaque fichier `*.toml` de `dir`, indexé par la clé `id`
/// que produit `key_of` sur la valeur désérialisée.
///
/// Un dossier absent produit une `HashMap` vide (ce n'est pas une erreur). Un
/// fichier illisible ou un TOML invalide produit une `Error::Config` qui
/// mentionne le chemin du fichier fautif.
fn load_toml_dir<T, F>(dir: &Path, key_of: F) -> crate::Result<HashMap<String, T>>
where
    T: for<'de> Deserialize<'de>,
    F: Fn(&T) -> String,
{
    let mut out = HashMap::new();

    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(err) => {
            return Err(crate::Error::Config(format!(
                "impossible de lire le dossier {} : {err}",
                dir.display()
            )));
        }
    };

    for entry in entries {
        let entry = entry.map_err(|err| {
            crate::Error::Config(format!(
                "impossible de lire une entrée du dossier {} : {err}",
                dir.display()
            ))
        })?;
        let path = entry.path();
        if path.extension().and_then(std::ffi::OsStr::to_str) != Some("toml") {
            continue;
        }

        let contents = std::fs::read_to_string(&path).map_err(|err| {
            crate::Error::Config(format!(
                "impossible de lire le fichier {} : {err}",
                path.display()
            ))
        })?;
        let value: T = toml::from_str(&contents).map_err(|err| {
            crate::Error::Config(format!("TOML invalide dans {} : {err}", path.display()))
        })?;
        out.insert(key_of(&value), value);
    }

    Ok(out)
}

/// Charge `<root>/backends/*.toml` et `<root>/models/*.toml`.
///
/// La clé de chaque table est le champ `id` du fichier.
pub fn load(root: &Path) -> crate::Result<Config> {
    let backends = load_toml_dir::<Backend, _>(&root.join("backends"), |b| b.id.clone())?;
    for backend in backends.values() {
        validate_backend(backend)?;
    }
    let models = load_toml_dir::<Model, _>(&root.join("models"), |m| m.id.clone())?;
    Ok(Config { backends, models })
}

impl Config {
    /// Résout un identifiant de modèle vers le couple `(Model, Backend)` correspondant.
    ///
    /// Renvoie `Error::Config` si le modèle est inconnu, ou si son backend n'existe
    /// pas ; dans les deux cas le message liste les identifiants disponibles.
    pub fn resolve(&self, model_id: &str) -> crate::Result<(&Model, &Backend)> {
        let Some(model) = self.models.get(model_id) else {
            return Err(crate::Error::Config(format!(
                "modèle inconnu : « {model_id} » (modèles disponibles : {})",
                crate::error::format_available(self.models.keys())
            )));
        };

        let Some(backend) = self.backends.get(&model.backend) else {
            return Err(crate::Error::Config(format!(
                "backend inconnu : « {} » (référencé par le modèle « {model_id} », backends disponibles : {})",
                model.backend,
                crate::error::format_available(self.backends.keys())
            )));
        };

        Ok((model, backend))
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)] // toléré dans les tests (cf. Cargo.toml [lints.clippy]).
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Crée un dossier de fixture unique sous `target/`, pour ne pas polluer le
    /// dépôt ni entrer en collision entre tests exécutés en parallèle.
    fn fixture_dir(name: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-fixtures")
            .join(format!("config-{name}-{n}"));
        std::fs::create_dir_all(&dir).expect("création du dossier de fixture");
        dir
    }

    fn write(dir: &Path, rel: &str, contents: &str) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("création du dossier parent");
        }
        std::fs::write(path, contents).expect("écriture de la fixture");
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

        let config = load(&root).expect("chargement de la config");
        let (model, backend) = config.resolve("gpt").expect("résolution du modèle");
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

        let config = load(&root).expect("chargement de la config");
        let err = config.resolve("inconnu").expect_err("doit échouer");
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

        let config = load(&root).expect("chargement de la config");
        let err = config.resolve("gpt").expect_err("doit échouer");
        assert!(matches!(err, crate::Error::Config(_)));
        let msg = err.to_string();
        assert!(msg.contains("absent"));
    }

    #[test]
    fn load_missing_directories_yields_empty_config() {
        let root = fixture_dir("load-missing-dirs");
        let config = load(&root).expect("un dossier absent n'est pas une erreur");
        assert!(config.backends.is_empty());
        assert!(config.models.is_empty());
    }

    #[test]
    fn load_invalid_toml_reports_file_path() {
        let root = fixture_dir("load-invalid-toml");
        write(&root, "backends/broken.toml", "not = [valid");

        let err = load(&root).expect_err("TOML invalide doit échouer");
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

        let err = load(&root).expect_err("une méthode non-POST doit être rejetée");
        assert!(matches!(err, crate::Error::Config(_)));
        let msg = err.to_string();
        assert!(msg.contains("ovms"));
        assert!(msg.contains("chat"));
        assert!(msg.contains("GET"));
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

        let err = load(&root).expect_err("un type de backend inconnu doit être rejeté");
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

        let err = load(&root).expect_err("une clé TOML inconnue sur un backend doit être rejetée");
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

        let err = load(&root).expect_err("une clé TOML inconnue sur un modèle doit être rejetée");
        assert!(matches!(err, crate::Error::Config(_)));
    }

    #[test]
    fn real_npu_fixture_still_parses() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".npu");
        let config = load(&root).expect("la fixture .npu/ réelle doit toujours charger");

        let backend = config
            .backends
            .get("ovms")
            .expect("le backend ovms doit être présent");
        assert_eq!(backend.kind, "openai-compatible");
        assert!(backend.operations.contains_key("chat"));

        let model = config
            .models
            .get("qwen-fast")
            .expect("le modèle qwen-fast doit être présent");
        assert_eq!(model.generation.temperature, Some(0.0));
        assert_eq!(model.generation.max_tokens, Some(512));
    }
}

//! Chargement de la configuration : backends et modèles.
//!
//! `load` charge un scope unique (cf. IMPLEMENTATION.md §2, phase 1).
//! `load_scopes` compose plusieurs scopes en couches (phase 2, décision 3) :
//! remplacement par `id`, le scope le plus local gagnant intégralement — pas
//! de fusion champ par champ (cf. npu-cli-spec.md §5).

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

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
///
/// Tourne APRÈS la fusion des scopes (cf. `load_scopes`), sur les entrées
/// survivantes uniquement : un backend invalide d'un scope général,
/// intégralement remplacé par un scope plus local, ne doit jamais atteindre
/// cette fonction (cf. revue L3, phase 2). `source` est le chemin du fichier
/// dont vient l'entrée survivante, pour que le message nomme le fichier que
/// l'utilisateur doit effectivement corriger.
fn validate_backend(backend: &Backend, source: &Path) -> crate::Result<()> {
    if backend.kind != SUPPORTED_BACKEND_KIND {
        return Err(crate::Error::Config(format!(
            "{} : backend « {} » : type « {} » non supporté (seul « {SUPPORTED_BACKEND_KIND} » \
             est supporté en phase 1)",
            source.display(),
            backend.id,
            backend.kind
        )));
    }

    for (operation_name, operation) in &backend.operations {
        if !operation.method.eq_ignore_ascii_case(SUPPORTED_METHOD) {
            return Err(crate::Error::Config(format!(
                "{} : backend « {} », opération « {operation_name} » : méthode « {} » non \
                 supportée (seule « {SUPPORTED_METHOD} » est supportée en phase 1)",
                source.display(),
                backend.id,
                operation.method
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
/// que produit `key_of` sur la valeur désérialisée. Chaque entrée est
/// accompagnée du chemin du fichier dont elle vient, pour que le scope le
/// plus local puisse transporter son origine jusqu'à la validation
/// sémantique qui tourne après la fusion (cf. `load_scopes`).
///
/// Un dossier absent produit une `HashMap` vide (ce n'est pas une erreur). Un
/// fichier illisible ou un TOML invalide produit une `Error::Config` qui
/// mentionne le chemin du fichier fautif — cette erreur de PARSING reste
/// fatale dans tous les scopes, contrairement à la validation sémantique :
/// avant que `key_of` ait pu être appelée, l'identité de l'entrée (donc la
/// question « est-elle masquée ? ») n'est pas connaissable (cf. revue L3,
/// phase 2).
///
/// Deux fichiers de `dir` qui déclarent le même `id` sont une ambiguïté de
/// configuration, pas une intention (cf. revue L3) : `Error::Config` nomme
/// l'identifiant en double et les deux chemins de fichiers concernés. Les
/// entrées du dossier sont triées avant lecture pour que ce diagnostic (quel
/// fichier est « le premier », quel fichier est « le doublon ») soit
/// déterministe plutôt que dépendant de l'ordre du système de fichiers.
///
/// Cette contrainte ne vaut qu'à l'intérieur de `dir` : `load_scopes` fusionne
/// plusieurs appels à cette fonction (un par scope) où le remplacement par
/// `id` est précisément la fonctionnalité demandée, pas une ambiguïté.
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
                "impossible de lire le dossier {} : {err}",
                dir.display()
            )));
        }
    };

    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| {
            crate::Error::Config(format!(
                "impossible de lire une entrée du dossier {} : {err}",
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
            crate::Error::Config(format!(
                "impossible de lire le fichier {} : {err}",
                path.display()
            ))
        })?;
        let value: T = toml::from_str(&contents).map_err(|err| {
            crate::Error::Config(format!("TOML invalide dans {} : {err}", path.display()))
        })?;
        let key = key_of(&value);

        if let Some(previous_path) = sources.get(&key) {
            return Err(crate::Error::Config(format!(
                "identifiant « {key} » défini plusieurs fois dans le même scope : {} et {} \
                 (dans un même scope, chaque identifiant doit être unique ; entre scopes, la \
                 redéfinition est la fonctionnalité attendue)",
                previous_path.display(),
                path.display()
            )));
        }

        sources.insert(key.clone(), path.clone());
        out.insert(key, (value, path));
    }

    Ok(out)
}

/// Charge `<root>/backends/*.toml` et `<root>/models/*.toml` : un simple
/// mono-scope, en termes de `load_scopes`. Ce n'est pas dupliqué en un
/// chemin de chargement distinct : mono-scope et multi-scope ne peuvent donc
/// jamais diverger sur le moment où la validation sémantique tourne (cf.
/// revue L3, phase 2).
///
/// La clé de chaque table est le champ `id` du fichier.
pub fn load(root: &Path) -> crate::Result<Config> {
    load_scopes(&[root.to_path_buf()])
}

/// Charge et fusionne plusieurs scopes de configuration (cf.
/// npu-cli-spec.md §5, IMPLEMENTATION.md décision 3).
///
/// `roots` doit être ordonné du plus général au plus local — c'est l'ordre
/// que produit `scope::roots()`. Chaque racine est chargée avec
/// `load_toml_dir`, puis fusionnée dans l'ordre reçu : pour `backends` comme
/// pour `models`, l'entrée d'un scope plus local remplace intégralement
/// celle de même `id` venue d'un scope plus général (`HashMap::extend`
/// appliqué dans l'ordre des racines — pas de fusion champ par champ, un
/// champ absent d'une redéfinition locale n'est pas hérité du scope
/// général).
///
/// La validation sémantique (`validate_backend`) tourne APRÈS cette fusion,
/// et seulement sur les entrées survivantes : un backend invalide d'un scope
/// général, intégralement masqué par un scope plus local, ne doit jamais
/// faire échouer le chargement (cf. revue L3, phase 2 — contrairement à une
/// erreur de PARSING TOML, qui reste fatale dans tous les scopes puisque
/// l'identité d'un fichier illisible n'est pas connaissable, donc son
/// masquage non plus : cf. `load_toml_dir`).
///
/// Une racine de `roots` qui n'existe pas sur le disque n'est pas une
/// erreur : `load_toml_dir` renvoie déjà des tables vides dans ce cas (un
/// dossier `NotFound` est traité comme vide), donc rien de spécial n'est
/// nécessaire ici. Une liste `roots` vide produit une `Config` vide.
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

        let err = load(&root).expect_err("un id dupliqué dans un même scope doit échouer");
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

        let config = load_scopes(&[general, local]).expect("la fusion de scopes doit réussir");
        let backend = config
            .backends
            .get("ovms")
            .expect("le backend ovms doit être présent");
        assert_eq!(backend.base_url, "http://local:8000");
        // Le champ absent de la redéfinition locale (embeddings) ne doit PAS
        // être hérité du scope général : remplacement intégral, pas de fusion
        // champ par champ.
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

        let config = load_scopes(&[general, local]).expect("la fusion de scopes doit réussir");
        let (model, backend) = config
            .resolve("qwen-fast")
            .expect("le modèle local doit résoudre le backend général");
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

        let config = load_scopes(&[missing, existing])
            .expect("une racine absente ne doit pas faire échouer la fusion");
        assert!(config.models.contains_key("gpt"));
    }

    #[test]
    fn load_scopes_empty_roots_yields_empty_config() {
        let config = load_scopes(&[]).expect("une liste de racines vide doit réussir");
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
            load_scopes(&[etc.clone(), xdg.clone(), cwd.clone()]).expect("la fusion doit réussir");
        assert_eq!(
            config.models.get("qwen-fast").expect("qwen-fast").model,
            "cwd-model",
            "la racine la plus locale (cwd, en fin de liste) doit gagner"
        );

        // Un ordre partiel (juste etc puis xdg, sans cwd) doit également
        // respecter général -> local.
        let config2 = load_scopes(&[etc, xdg]).expect("la fusion à deux racines doit réussir");
        assert_eq!(
            config2.models.get("qwen-fast").expect("qwen-fast").model,
            "xdg-model"
        );
    }

    #[test]
    fn load_scopes_invalid_backend_fully_masked_by_local_scope_resolves_successfully() {
        // Revue L3 (phase 2) : un backend invalide d'un scope général,
        // intégralement remplacé par un scope local valide, ne doit jamais
        // atteindre la validation sémantique.
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
            .expect("le backend invalide masqué ne doit pas empêcher la résolution");
        let backend = config
            .backends
            .get("ovms")
            .expect("le backend ovms (version locale) doit être présent");
        assert_eq!(backend.kind, "openai-compatible");
        assert_eq!(backend.base_url, "http://local:8000");
    }

    #[test]
    fn load_scopes_invalid_backend_not_masked_still_fails_and_names_its_file() {
        // Le cas inverse, le plus facile à casser en corrigeant le premier :
        // un backend invalide présent UNIQUEMENT dans un scope général doit
        // toujours échouer, et le message doit nommer son fichier (pas
        // seulement son id), puisque la validation tourne maintenant après
        // la fusion sur une entrée qui transporte son origine.
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

        let err = load_scopes(&[general])
            .expect_err("un backend invalide non masqué doit toujours échouer");
        assert!(matches!(err, crate::Error::Config(_)));
        let msg = err.to_string();
        assert!(
            msg.contains("ovms.toml"),
            "le message doit nommer le fichier fautif, obtenu : {msg}"
        );
        assert!(msg.contains("ollama"));
    }

    #[test]
    fn load_scopes_toml_parse_error_in_general_scope_remains_fatal_even_when_masked() {
        // Contrairement à la validation sémantique, une erreur de PARSING
        // reste fatale dans tous les scopes : un fichier illisible n'a pas
        // d'identité connaissable, donc on ne peut pas savoir s'il est
        // masqué (cf. revue L3, phase 2). Ce n'est pas un bug.
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
            "une erreur de parsing TOML dans un scope général doit rester fatale même \
             lorsqu'un id valide existe en local",
        );
        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("ovms.toml"));
    }
}

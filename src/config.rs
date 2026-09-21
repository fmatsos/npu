//! Chargement de la configuration : backends et modèles.
//!
//! Phase 1 : un seul scope (`./.npu`), pas de fusion de scopes (cf. IMPLEMENTATION.md §2).

use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

/// Une opération HTTP exposée par un backend (ex. `chat`).
#[derive(Debug, Deserialize)]
pub struct Operation {
    pub method: String,
    pub path: String,
}

/// Un backend IA configuré dans `backends/*.toml`.
#[derive(Debug, Deserialize)]
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
pub struct Generation {
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

/// Un modèle configuré dans `models/*.toml`, référençant un backend.
#[derive(Debug, Deserialize)]
pub struct Model {
    pub id: String,
    pub backend: String,
    pub operation: String,
    pub model: String,
    #[serde(default)]
    pub generation: Generation,
}

/// Configuration résolue : backends et modèles indexés par leur `id`.
#[derive(Debug, Default)]
pub struct Config {
    pub backends: HashMap<String, Backend>,
    pub models: HashMap<String, Model>,
}

/// Charge `<root>/backends/*.toml` et `<root>/models/*.toml`.
///
/// La clé de chaque table est le champ `id` du fichier.
pub fn load(root: &std::path::Path) -> crate::Result<Config> {
    let _ = root;
    todo!()
}

impl Config {
    /// Résout un identifiant de modèle vers le couple `(Model, Backend)` correspondant.
    pub fn resolve(&self, model_id: &str) -> crate::Result<(&Model, &Backend)> {
        let _ = model_id;
        todo!()
    }
}

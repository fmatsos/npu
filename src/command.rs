//! Découverte et parsing des commandes définies sous `commands/**/*.md`.
#![allow(clippy::todo)] // stub : implémentation à l'étape suivante.

use serde::Deserialize;

/// Mode de résolution de l'entrée d'une commande.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputMode {
    Stdin,
    File,
    StdinOrFile,
}

/// Une commande découverte : son chemin (dérivé de l'arborescence), le modèle
/// à utiliser, le mode d'entrée et le prompt (corps du fichier).
#[derive(Debug)]
pub struct CommandSpec {
    pub path: Vec<String>,
    pub description: String,
    pub model: String,
    pub input: InputMode,
    pub prompt: String,
}

/// Parcourt `<root>/commands/**/*.md`.
///
/// Renvoie un `Vec` vide si le dossier `commands/` n'existe pas.
pub fn discover(root: &std::path::Path) -> crate::Result<Vec<CommandSpec>> {
    let _ = root;
    todo!()
}

/// Parse le contenu d'un fichier de commande.
///
/// Le frontmatter est délimité par des lignes `+++` ; l'en-tête est du TOML,
/// le corps (après le second délimiteur) est le prompt.
pub fn parse(source: &str, path: Vec<String>) -> crate::Result<CommandSpec> {
    let _ = (source, path);
    todo!()
}

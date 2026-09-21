//! Moteur générique d'exécution de commandes IA locales.
//!
//! Toute la logique vit ici : le binaire (`main.rs`) n'est qu'une coquille.
//! C'est ce qui rend le pipeline testable depuis `tests/`, qui ne peut importer
//! qu'une cible bibliothèque (cf. IMPLEMENTATION.md §4).

pub mod backend;
pub mod command;
pub mod config;
pub mod error;
pub mod input;
pub mod prompt;

pub use error::{Error, Result};

/// Point d'entrée de la bibliothèque, appelé par `main`.
#[allow(clippy::todo)] // stub : implémentation à l'étape suivante.
pub fn run() -> Result<()> {
    todo!()
}

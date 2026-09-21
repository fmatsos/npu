//! Adaptateur de protocole `openai-compatible`, opération `chat` uniquement.
#![allow(clippy::todo)] // stub : implémentation à l'étape suivante.

/// Exécute l'opération `chat` du `model` donné auprès de `backend`, avec `prompt`.
pub fn chat(
    backend: &crate::config::Backend,
    model: &crate::config::Model,
    prompt: &str,
) -> crate::Result<String> {
    let _ = (backend, model, prompt);
    todo!()
}

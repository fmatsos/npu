//! Résolution de l'entrée d'une commande : stdin, fichier, ou l'un ou l'autre.
#![allow(clippy::todo)] // stub : implémentation à l'étape suivante.

/// Résout l'entrée selon le `mode` de la commande et le fichier éventuellement
/// fourni en argument.
pub fn resolve(
    mode: &crate::command::InputMode,
    file: Option<&std::path::Path>,
) -> crate::Result<String> {
    let _ = (mode, file);
    todo!()
}

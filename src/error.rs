//! Erreur unifiée du crate.
//!
//! Un enum écrit à la main plutôt qu'un `anyhow`/`thiserror` : quatre variantes,
//! un code de sortie chacune (cf. IMPLEMENTATION.md §1.7 / npu-cli-spec.md §14).
//! stdout reste réservé au résultat de la commande ; ces messages sont destinés
//! à stderr.

use std::fmt;

/// Erreur unifiée du crate, une variante par famille de code de sortie.
#[derive(Debug)]
pub enum Error {
    /// Configuration invalide ou introuvable (backends/modèles/commandes).
    Config(String),
    /// Échec côté backend IA (requête, réseau, réponse inattendue).
    Backend(String),
    /// Échec de production ou de validation de la sortie.
    Output(String),
    /// Erreur d'entrée/sortie système (fichier, stdin, ...).
    Io(std::io::Error),
}

impl Error {
    /// Code de sortie process associé à cette erreur.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Io(_) => 1,
            Error::Config(_) => 2,
            Error::Backend(_) => 3,
            Error::Output(_) => 4,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Config(msg) => write!(f, "erreur de configuration : {msg}"),
            Error::Backend(msg) => write!(f, "erreur backend : {msg}"),
            Error::Output(msg) => write!(f, "erreur de sortie : {msg}"),
            Error::Io(err) => write!(f, "erreur d'entrée/sortie : {err}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(err) => Some(err),
            Error::Config(_) | Error::Backend(_) | Error::Output(_) => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::Io(err)
    }
}

/// Alias de résultat du crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Formate une liste d'identifiants disponibles pour un message d'erreur
/// actionnable : triée, jointe par virgule, ou `"aucune"` si vide.
///
/// Centralise un idiome répété à l'identique dans `config::resolve` (modèles,
/// backends), `backend::chat` (opérations) et `run` (commandes), pour que les
/// messages d'erreur restent homogènes d'un module à l'autre.
pub(crate) fn format_available<S: AsRef<str>>(ids: impl Iterator<Item = S>) -> String {
    let mut sorted: Vec<String> = ids.map(|s| s.as_ref().to_string()).collect();
    sorted.sort_unstable();
    if sorted.is_empty() {
        "aucune".to_string()
    } else {
        sorted.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_match_contract() {
        assert_eq!(Error::Io(std::io::Error::other("x")).exit_code(), 1);
        assert_eq!(Error::Config("x".into()).exit_code(), 2);
        assert_eq!(Error::Backend("x".into()).exit_code(), 3);
        assert_eq!(Error::Output("x".into()).exit_code(), 4);
    }

    #[test]
    fn display_includes_message() {
        let err = Error::Config("champ manquant".to_string());
        assert!(err.to_string().contains("champ manquant"));
    }

    #[test]
    fn from_io_error_wraps() {
        let io_err = std::io::Error::other("boom");
        let err: Error = io_err.into();
        assert!(matches!(err, Error::Io(_)));
    }

    #[test]
    fn format_available_sorts_and_joins() {
        assert_eq!(format_available(["b", "a", "c"].into_iter()), "a, b, c");
    }

    #[test]
    fn format_available_empty_is_aucune() {
        assert_eq!(format_available(std::iter::empty::<&str>()), "aucune");
    }
}

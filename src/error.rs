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
    fn config_and_output_errors_render_with_distinct_unmistakable_prefixes() {
        // Exigence L2 explicite de la revue de phase 4 : un utilisateur doit
        // comprendre IMMÉDIATEMENT, à la lecture du message, si c'est SA
        // configuration ou la réponse du MODÈLE qui est en cause. Ce test
        // fige les deux préfixes ET prouve qu'ils ne peuvent pas être
        // confondus l'un avec l'autre.
        let config_msg = Error::Config("schéma introuvable".to_string()).to_string();
        let output_msg = Error::Output("JSON invalide".to_string()).to_string();

        assert!(
            config_msg.starts_with("erreur de configuration : "),
            "obtenu : {config_msg}"
        );
        assert!(
            output_msg.starts_with("erreur de sortie : "),
            "obtenu : {output_msg}"
        );
        assert_ne!(
            config_msg.split(':').next(),
            output_msg.split(':').next(),
            "les préfixes de Error::Config et Error::Output ne doivent jamais coïncider, \
             sous peine qu'un utilisateur ne puisse plus distinguer une configuration cassée \
             d'une réponse de modèle invalide à la seule lecture du message"
        );
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

//! Résolution de l'entrée d'une commande : stdin, fichier, ou l'un ou l'autre.

use std::io::Read;

/// Résout l'entrée selon le `mode` de la commande et le fichier éventuellement
/// fourni en argument.
pub fn resolve(
    mode: &crate::command::InputMode,
    file: Option<&std::path::Path>,
) -> crate::Result<String> {
    match mode {
        crate::command::InputMode::Stdin => {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            Ok(buf)
        }
        crate::command::InputMode::File => {
            let path = file.ok_or_else(|| {
                crate::Error::Config("cette commande attend un fichier en argument".to_string())
            })?;
            std::fs::read_to_string(path).map_err(Into::into)
        }
        crate::command::InputMode::StdinOrFile => {
            if let Some(path) = file {
                std::fs::read_to_string(path).map_err(Into::into)
            } else {
                let mut buf = String::new();
                std::io::stdin().read_to_string(&mut buf)?;
                Ok(buf)
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)] // toléré dans les tests (cf. Cargo.toml [lints.clippy]) ; même
// convention que dans lib.rs/command.rs/config.rs.
mod tests {
    use super::*;

    #[test]
    fn file_mode_with_valid_path() {
        // Crée un fichier temporaire dans target/ avec un contenu connu.
        let test_file_path = std::path::Path::new("target/npu-input-test-file.txt");
        let test_content = "Hello from test file\n";

        // Écrit le fichier
        let write_result = std::fs::write(test_file_path, test_content);
        assert!(write_result.is_ok(), "Failed to write test file");

        // Résout le fichier en mode File
        let mode = crate::command::InputMode::File;
        let result = resolve(&mode, Some(test_file_path));

        // Vérifie le résultat
        assert!(result.is_ok(), "resolve() should succeed");
        if let Ok(content) = result {
            assert_eq!(content, test_content);
        }

        // Nettoie le fichier
        let _ = std::fs::remove_file(test_file_path);
    }

    #[test]
    fn stdin_or_file_mode_reads_the_file_when_a_path_is_given() {
        // Couvre la branche `StdinOrFile` avec fichier fourni, distincte de
        // `File` : sans ce test, un `StdinOrFile` qui ignorerait le fichier et
        // lirait toujours stdin ne serait détecté par aucun test existant.
        let test_file_path = std::path::Path::new("target/npu-input-test-stdin-or-file.txt");
        let test_content = "Hello from stdin_or_file test\n";
        std::fs::write(test_file_path, test_content).expect("écriture du fichier de test");

        let mode = crate::command::InputMode::StdinOrFile;
        let result = resolve(&mode, Some(test_file_path));

        assert_eq!(result.expect("resolve() doit réussir"), test_content);

        let _ = std::fs::remove_file(test_file_path);
    }

    #[test]
    fn file_mode_without_path_returns_config_error() {
        let mode = crate::command::InputMode::File;
        let result = resolve(&mode, None);

        assert!(
            matches!(result, Err(crate::Error::Config(ref msg)) if msg.contains("fichier")),
            "Expected Config error with 'fichier' message"
        );
    }
}

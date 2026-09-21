//! Resolution of a command's input: stdin, file, or either.

use std::io::Read;

/// Resolves the input according to the command's `mode` and the file
/// optionally given as an argument.
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
                crate::Error::Config("this command expects a file argument".to_string())
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
#[allow(clippy::expect_used)] // tolerated in tests (see Cargo.toml [lints.clippy]); same
// convention as in lib.rs/command.rs/config.rs.
mod tests {
    use super::*;

    #[test]
    fn file_mode_with_valid_path() {
        // Creates a temporary file in target/ with known content.
        let test_file_path = std::path::Path::new("target/npu-input-test-file.txt");
        let test_content = "Hello from test file\n";

        // Writes the file
        let write_result = std::fs::write(test_file_path, test_content);
        assert!(write_result.is_ok(), "Failed to write test file");

        // Resolves the file in File mode
        let mode = crate::command::InputMode::File;
        let result = resolve(&mode, Some(test_file_path));

        // Checks the result
        assert!(result.is_ok(), "resolve() should succeed");
        if let Ok(content) = result {
            assert_eq!(content, test_content);
        }

        // Cleans up the file
        let _ = std::fs::remove_file(test_file_path);
    }

    #[test]
    fn stdin_or_file_mode_reads_the_file_when_a_path_is_given() {
        // Covers the `StdinOrFile` branch with a file given, distinct from
        // `File`: without this test, a `StdinOrFile` that ignored the file and
        // always read stdin would not be caught by any existing test.
        let test_file_path = std::path::Path::new("target/npu-input-test-stdin-or-file.txt");
        let test_content = "Hello from stdin_or_file test\n";
        std::fs::write(test_file_path, test_content).expect("test file write");

        let mode = crate::command::InputMode::StdinOrFile;
        let result = resolve(&mode, Some(test_file_path));

        assert_eq!(result.expect("resolve() must succeed"), test_content);

        let _ = std::fs::remove_file(test_file_path);
    }

    #[test]
    fn file_mode_without_path_returns_config_error() {
        let mode = crate::command::InputMode::File;
        let result = resolve(&mode, None);

        assert!(
            matches!(result, Err(crate::Error::Config(_))),
            "Expected Config error"
        );
    }
}

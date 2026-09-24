//! Resolution of a command's input: stdin, file, or either.

use std::io::Read;

/// The largest input `npu` reads: a prompt is sent whole to a local model,
/// and anything past this is a mistake (a binary, a disk image) rather than
/// text a context window could hold.
const MAX_INPUT_BYTES: u64 = 64 * 1024 * 1024;

/// Resolves the input according to the command's `mode` and the file
/// optionally given as an argument. Every I/O error names its source (the
/// file path, or `stdin`), since the bare `std` message names neither.
pub fn resolve(
    mode: &crate::command::InputMode,
    file: Option<&std::path::Path>,
) -> crate::Result<String> {
    match (mode, file) {
        (crate::command::InputMode::Stdin, _) | (crate::command::InputMode::StdinOrFile, None) => {
            read_capped(std::io::stdin().lock(), "stdin")
        }
        (crate::command::InputMode::File, None) => Err(crate::Error::Config(
            "this command expects a file argument".to_string(),
        )),
        (_, Some(path)) => {
            let source = path.display().to_string();
            let reader = std::fs::File::open(path).map_err(|e| named(&source, &e))?;
            read_capped(reader, &source)
        }
    }
}

/// Reads `reader` as UTF-8, refusing more than [`MAX_INPUT_BYTES`]. The
/// size is checked on bytes before decoding, so a cap that splits a
/// multibyte character still reports the size, not invalid UTF-8.
fn read_capped(reader: impl Read, source: &str) -> crate::Result<String> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_INPUT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| named(source, &e))?;
    if bytes.len() as u64 > MAX_INPUT_BYTES {
        return Err(crate::Error::Io(std::io::Error::new(
            std::io::ErrorKind::FileTooLarge,
            format!("{source}: input exceeds {MAX_INPUT_BYTES} bytes"),
        )));
    }
    String::from_utf8(bytes).map_err(|e| {
        named(
            source,
            &std::io::Error::new(std::io::ErrorKind::InvalidData, e.utf8_error()),
        )
    })
}

fn named(source: &str, e: &std::io::Error) -> crate::Error {
    crate::Error::Io(std::io::Error::new(e.kind(), format!("{source}: {e}")))
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

    #[test]
    fn a_missing_file_is_an_io_error_naming_the_path() {
        let path = std::path::Path::new("target/npu-input-does-not-exist.txt");
        let err = resolve(&crate::command::InputMode::File, Some(path)).expect_err("missing");
        assert!(matches!(err, crate::Error::Io(_)));
        assert!(err.to_string().contains("npu-input-does-not-exist.txt"));
    }

    #[test]
    fn non_utf8_input_is_an_io_error_naming_its_source() {
        let err = read_capped(&[0xff, 0xfe][..], "stdin").expect_err("invalid utf-8");
        assert!(matches!(err, crate::Error::Io(_)));
        assert!(err.to_string().contains("stdin"));
    }

    #[test]
    fn input_past_the_cap_is_refused() {
        let big = std::io::repeat(b'a').take(MAX_INPUT_BYTES + 1);
        assert!(matches!(
            read_capped(big, "stdin"),
            Err(crate::Error::Io(_))
        ));
        let exact = std::io::repeat(b'a').take(MAX_INPUT_BYTES);
        assert!(read_capped(exact, "stdin").is_ok());
    }
}

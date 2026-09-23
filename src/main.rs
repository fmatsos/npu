//! Shell of the binary: all the logic lives in `lib.rs`.

/// `npu::run()` returns an exit code (`Ok(i32)`), not just a boolean
/// success/failure: `Ok(0)` is ordinary success, `Ok(code)` for `code != 0`
/// carries the exit code of a REPORT (`npu doctor` — cf. `npu::run`'s doc),
/// already written to stdout by `run` itself before returning here — this
/// is not an engine failure, so `main` must neither write it a second time
/// nor write it to stderr. `Err` remains a pipeline failure (§14: message
/// on stderr, code via `Error::exit_code`).
fn main() {
    match npu::run() {
        Ok(0) => {}
        Ok(code) => std::process::exit(code),
        Err(err) => {
            // `anstream` strips the colour when stderr is not a terminal.
            anstream::eprintln!("{}", npu::style::paint(npu::style::ERROR, &err.to_string()));
            std::process::exit(err.exit_code());
        }
    }
}

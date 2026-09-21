//! Coquille du binaire : toute la logique vit dans `lib.rs`.

fn main() {
    if let Err(err) = npu::run() {
        eprintln!("{err}");
        std::process::exit(err.exit_code());
    }
}

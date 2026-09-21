//! Coquille du binaire : toute la logique vit dans `lib.rs`.

/// `npu::run()` renvoie un code de sortie (`Ok(i32)`), pas seulement un
/// succès/échec booléen : `Ok(0)` est le succès ordinaire, `Ok(code)` pour
/// `code != 0` transporte le code de sortie d'un RAPPORT (`npu doctor` —
/// cf. doc de `npu::run`), déjà écrit sur stdout par `run` elle-même avant
/// de renvoyer ici — ce n'est pas un échec du moteur, donc `main` ne doit ni
/// l'écrire une seconde fois ni l'écrire sur stderr. `Err` reste un échec du
/// pipeline (§14 : message sur stderr, code via `Error::exit_code`).
fn main() {
    match npu::run() {
        Ok(0) => {}
        Ok(code) => std::process::exit(code),
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(err.exit_code());
        }
    }
}

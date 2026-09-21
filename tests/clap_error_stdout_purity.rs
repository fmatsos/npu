//! Verrouille la pureté de stdout (npu-cli-spec.md §14) sur le chemin
//! d'erreur de `clap` LUI-MÊME (sous-commande inconnue, argument requis
//! manquant), qui sort via `get_matches()` — donc via `std::process::exit`
//! interne à `clap` — SANS passer par le gestionnaire d'erreurs de `main()`
//! (`src/main.rs`).
//!
//! Preuve demandée par la revue L1+L2 (lacune non couverte par
//! `output_contract_e2e.rs`, qui ne vérifie la pureté de stdout que sur le
//! chemin d'erreur applicatif — `Error::Config`/`Error::Output`, géré par
//! `main()` — jamais sur celui de `clap`) : §23 fait de `npu` un CLI appelé
//! par un agent, pour qui une sortie non vide sur stdout en cas d'échec est
//! aussi dangereuse ici que sur n'importe quel autre chemin d'erreur — un
//! agent qui pipe stdout ne doit jamais recevoir un message d'usage `clap`
//! mélangé à une sortie de commande.
//!
//! Utilise le VRAI binaire `npu` compilé (`env!("CARGO_BIN_EXE_npu")`),
//! exécuté avec la fixture `.npu/` versionnée du dépôt (répertoire courant =
//! racine du crate, cf. `CARGO_MANIFEST_DIR`) comme scope local — même
//! fixture que `tests/cli.rs`. `HOME` est redirigé vers un répertoire
//! temporaire sans `.config/npu` et `XDG_CONFIG_HOME` est retiré, pour que
//! seule cette fixture locale soit prise en compte (même idiome
//! d'isolation que `tests/output_contract_e2e.rs`) ; aucun réseau n'est
//! jamais contacté par ces trois scénarios, `clap` échouant (ou `--help`
//! s'imprimant) avant tout appel de backend.

#![allow(clippy::expect_used)] // toléré dans les tests (cf. Cargo.toml [lints.clippy]).

use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

fn isolated_home() -> std::path::PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-fixtures")
        .join(format!("clap-stdout-purity-home-{n}"));
    std::fs::create_dir_all(&dir).expect("création du répertoire HOME isolé");
    dir
}

fn run_npu(args: &[&str]) -> Output {
    let home = isolated_home();
    Command::new(env!("CARGO_BIN_EXE_npu"))
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("lancement du binaire npu")
        .wait_with_output()
        .expect("attente de la fin du processus npu")
}

/// Sous-commande inexistante : `clap` échoue AVANT que `main()` ne
/// reprenne la main (`get_matches()` appelle `std::process::exit`
/// directement, cf. `lib.rs::run`). stdout doit rester rigoureusement vide.
#[test]
fn unknown_subcommand_writes_nothing_to_stdout() {
    let output = run_npu(&["sous-commande-inexistante"]);

    assert!(
        !output.status.success(),
        "une sous-commande inconnue doit échouer"
    );
    assert!(
        output.stdout.is_empty(),
        "stdout doit rester vide sur une erreur `clap` (sous-commande inconnue), obtenu : {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8(output.stderr).expect("stderr doit être de l'UTF-8 valide");
    assert!(
        stderr.contains("sous-commande-inexistante"),
        "stderr doit nommer la sous-commande fautive, obtenu : {stderr}"
    );
}

/// Argument requis manquant (`translate` sans `--language`) : même chemin
/// d'erreur `clap`, une autre forme (validation d'arguments plutôt que
/// résolution de sous-commande). stdout doit rester rigoureusement vide.
#[test]
fn missing_required_arg_writes_nothing_to_stdout() {
    let output = run_npu(&["translate"]);

    assert!(
        !output.status.success(),
        "translate sans --language doit échouer (argument requis manquant)"
    );
    assert!(
        output.stdout.is_empty(),
        "stdout doit rester vide sur une erreur `clap` (argument requis manquant), obtenu : {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8(output.stderr).expect("stderr doit être de l'UTF-8 valide");
    assert!(
        stderr.to_lowercase().contains("language"),
        "stderr doit nommer l'argument requis manquant, obtenu : {stderr}"
    );
}

/// Cas nominal, pour contraste explicite : `--help` DOIT écrire sur stdout
/// (ce n'est pas un chemin d'erreur, cf. `clap`'s comportement standard) —
/// verrouille que ce test ne confond pas « stdout vide » avec « `clap` ne
/// doit jamais rien écrire sur stdout », ce qui serait faux.
#[test]
fn help_writes_to_stdout_not_stderr() {
    let output = run_npu(&["--help"]);

    assert!(output.status.success(), "npu --help doit réussir");
    assert!(
        !output.stdout.is_empty(),
        "npu --help DOIT écrire l'aide sur stdout, ce n'est pas un chemin d'erreur"
    );
    assert!(
        output.stderr.is_empty(),
        "npu --help ne doit rien écrire sur stderr, obtenu : {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

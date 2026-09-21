//! Tests d'intégration.
//!
//! - `discovers_commit_message_fixture` : vérifie que `command::discover`
//!   retrouve la fixture `.npu/commands/commit-message.md` avec le bon modèle
//!   et le bon mode d'entrée.
//! - `layered_scopes_local_wins_over_general` (phase 2) : construit deux
//!   racines de scope temporaires sous `target/` (une « générale », une
//!   « locale », toutes deux distinctes de la fixture versionnée `.npu/`) et
//!   vérifie que `discover_scopes` et `load_scopes` retiennent bien la
//!   version locale d'une commande ET d'un modèle définis dans les deux.
//!
//! Aucun de ces tests n'appelle le réseau ni ne touche aux vraies variables
//! d'environnement : `discover_scopes`/`load_scopes` sont des fonctions pures
//! sur une liste de racines passée explicitement, jamais `scope::roots()`.
#![allow(clippy::expect_used)] // toléré dans les tests (cf. Cargo.toml [lints.clippy]).

use npu::command::{self, InputMode};
use npu::config;
use std::sync::atomic::{AtomicU64, Ordering};

#[test]
fn discovers_commit_message_fixture() {
    let root = std::path::Path::new(".npu");
    let commands = command::discover(root).expect("la découverte des commandes doit réussir");

    let commit_message = commands
        .iter()
        .find(|spec| spec.path == vec!["commit-message".to_string()])
        .expect("commit-message doit être découverte");

    assert_eq!(commit_message.model, "qwen-fast");
    assert!(matches!(commit_message.input, InputMode::Stdin));
}

/// Crée un dossier de fixture unique sous `target/`, pour ne pas polluer le
/// dépôt ni entrer en collision entre tests exécutés en parallèle (même
/// idiome que `config::tests::fixture_dir` / `command::tests::fixture_dir`).
fn fixture_dir(name: &str) -> std::path::PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-fixtures")
        .join(format!("cli-scopes-{name}-{n}"))
}

fn write(dir: &std::path::Path, rel: &str, contents: &str) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("création du dossier parent");
    }
    std::fs::write(path, contents).expect("écriture de la fixture");
}

#[test]
fn layered_scopes_local_wins_over_general() {
    let general = fixture_dir("general");
    let local = fixture_dir("local");

    // Scope général : une commande et un modèle (avec son backend).
    write(
        &general,
        "commands/classify.md",
        "+++\ndescription = \"Classify (général)\"\nmodel = \"qwen-fast\"\n+++\nprompt général\n",
    );
    write(
        &general,
        "backends/ovms.toml",
        r#"
        id = "ovms"
        base_url = "http://general:8000"
        type = "openai-compatible"

        [operations.chat]
        method = "POST"
        path = "/v3/chat/completions"
        "#,
    );
    write(
        &general,
        "models/qwen-fast.toml",
        r#"
        id = "qwen-fast"
        backend = "ovms"
        operation = "chat"
        model = "qwen-general"
        "#,
    );

    // Scope local : redéfinit la même commande et le même modèle.
    write(
        &local,
        "commands/classify.md",
        "+++\ndescription = \"Classify (local)\"\nmodel = \"qwen-fast\"\n+++\nprompt local\n",
    );
    write(
        &local,
        "models/qwen-fast.toml",
        r#"
        id = "qwen-fast"
        backend = "ovms"
        operation = "chat"
        model = "qwen-local"
        "#,
    );

    // Ordre général -> local, exactement celui que produit `scope::roots()`.
    let roots = vec![general, local];

    let specs = command::discover_scopes(&roots).expect("la découverte en couches doit réussir");
    let classify = specs
        .iter()
        .find(|spec| spec.path == vec!["classify".to_string()])
        .expect("classify doit être découverte");
    assert_eq!(classify.prompt, "prompt local");
    assert!(matches!(classify.input, InputMode::Stdin));

    let config = config::load_scopes(&roots).expect("le chargement en couches doit réussir");
    let (model, backend) = config
        .resolve("qwen-fast")
        .expect("qwen-fast doit résoudre, le backend étant hérité du scope général");
    assert_eq!(
        model.model, "qwen-local",
        "le modèle local doit remplacer intégralement le modèle général"
    );
    assert_eq!(backend.id, "ovms");
}

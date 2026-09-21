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
use npu::prompt;
use std::collections::BTreeSet;
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

/// Découvre la fixture versionnée `.npu/commands/translate.md` (phase 3,
/// npu-cli-spec.md §11) : vérifie que l'argument `language` est déclaré avec
/// la bonne lettre courte et le bon caractère `required`, et que son prompt
/// (qui référence `{{ args.language }}`) passe bien la validation statique
/// des placeholders (§12). N'appelle pas le réseau : `command::discover` ne
/// fait que lire et parser des fichiers locaux.
#[test]
fn discovers_translate_fixture_with_declared_language_arg() {
    let root = std::path::Path::new(".npu");
    let commands = command::discover(root).expect("la découverte des commandes doit réussir");

    let translate = commands
        .iter()
        .find(|spec| spec.path == vec!["translate".to_string()])
        .expect("translate doit être découverte");

    let language = translate
        .args
        .get("language")
        .expect("l'argument 'language' doit être déclaré");
    assert_eq!(language.short, Some('l'));
    assert!(language.required, "'language' doit être required = true");

    let declared: BTreeSet<String> = translate.args.keys().cloned().collect();
    prompt::validate(&translate.prompt, &declared)
        .expect("le prompt de translate doit passer la validation statique des placeholders");
}

/// Découvre la fixture versionnée `.npu/commands/classify.md` (phase 4,
/// npu-cli-spec.md §6/§15) : vérifie que la commande déclare bien
/// `format = "json"` ET que le chemin de schéma résolu (`schemas/
/// classification.json`, relatif à la racine de scope `.npu/`) EXISTE
/// réellement sur le disque — la dette précise que cette phase rembourse
/// (`[output]` lu puis honoré, pas seulement accepté). N'appelle pas le
/// réseau : `command::discover` ne fait que lire et parser des fichiers
/// locaux.
#[test]
fn discovers_classify_fixture_with_json_format_and_existing_schema() {
    let root = std::path::Path::new(".npu");
    let commands = command::discover(root).expect("la découverte des commandes doit réussir");

    let classify = commands
        .iter()
        .find(|spec| spec.path == vec!["classify".to_string()])
        .expect("classify doit être découverte");

    assert_eq!(
        classify.output.format,
        npu::output::Format::Json,
        "classify doit déclarer format = \"json\""
    );
    let schema_path = classify
        .output
        .schema
        .as_deref()
        .expect("classify doit déclarer un chemin de schéma");
    assert!(
        schema_path.is_file(),
        "le chemin de schéma résolu doit exister réellement sur le disque, obtenu : {}",
        schema_path.display()
    );
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

//! Vérification de bout en bout de la phase 5 (built-ins + mode dégradé,
//! npu-cli-spec.md §16, IMPLEMENTATION.md phase 5) — le VRAI binaire
//! (`env!("CARGO_BIN_EXE_npu")`), jamais une fonction appelée directement
//! dans ce processus de test, avec des scopes temporaires montés via
//! `$XDG_CONFIG_HOME` (même idiome que `run_npu_xdg`/
//! `fixture_cwd_without_local_scope` dans `tests/output_contract_e2e.rs`).
//!
//! Deux familles de scénarios :
//! - (a)/(b)/(c) : une configuration CASSÉE (un TOML de backend illisible)
//!   -> `--help` reste utilisable (mode dégradé), `doctor` rapporte l'échec
//!   de chargement (code 2), toute autre invocation propage cette même
//!   erreur (code 2) ;
//! - (d)/(e)/(f) : une configuration SAINE -> `doctor` distingue une
//!   configuration valide d'un backend éteint (code 3, jamais 2), `models`
//!   et `describe` produisent leur sortie attendue sur stdout (code 0).
//!
//! Aucun de ces scénarios n'appelle un vrai backend réseau : `--help`,
//! `doctor`, `models` et `describe` ne contactent jamais un backend (`doctor`
//! ouvre au plus un socket TCP vers un port délibérément fermé, cf. (d)).
//! Aucun test n'utilise le port 8000 : c'est celui de la fixture bouchonnée
//! du dépôt (`tests/output_contract_e2e.rs`), et un service local qui y
//! répondrait fausserait silencieusement (d). Le port fermé de (d) est
//! obtenu en liant un `TcpListener` éphémère puis en le fermant aussitôt
//! (même idiome que `builtin::tests::tcp_probe_fails_against_a_closed_port`),
//! jamais un numéro de port codé en dur.
//!
//! `HOME` est redirigé vers un répertoire temporaire sans `.config/npu` pour
//! chaque invocation, `$XDG_CONFIG_HOME` pointe sur le scope temporaire écrit
//! par le test : seule cette racine de scope est prise en compte par
//! `scope::roots()`, jamais le vrai `$HOME` ni un `/etc/npu` qui existerait
//! par ailleurs sur la machine. `Command::env`/`env_remove` ne touchent que
//! l'environnement du PROCESSUS ENFANT : aucun test ne mute les vraies
//! variables d'environnement (`std::env::set_var` est `unsafe` en édition
//! 2024, interdit par `unsafe_code = "forbid"`, cf. Cargo.toml).

#![allow(clippy::expect_used)] // toléré dans les tests (cf. Cargo.toml [lints.clippy]).

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

/// Crée un répertoire unique sous `target/`, pour ne pas polluer le dépôt ni
/// entrer en collision entre tests exécutés en parallèle (même idiome que
/// les autres fichiers de `tests/`).
fn fixture_dir(name: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-fixtures")
        .join(format!("phase5-{name}-{n}"));
    std::fs::create_dir_all(&dir).expect("création du répertoire de fixture");
    dir
}

fn write(dir: &Path, rel: &str, contents: &str) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("création du dossier parent");
    }
    std::fs::write(path, contents).expect("écriture de la fixture");
}

/// Exécute le VRAI binaire `npu` avec `$XDG_CONFIG_HOME` pointé sur
/// `xdg_config_home` et `cwd` (délibérément sans `.npu` local) comme
/// répertoire courant — même idiome que `run_npu_xdg` dans
/// `tests/output_contract_e2e.rs`.
fn run_npu(cwd: &Path, xdg_config_home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_npu"))
        .args(args)
        .current_dir(cwd)
        .env("HOME", cwd)
        .env("XDG_CONFIG_HOME", xdg_config_home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("lancement du binaire npu")
        .wait_with_output()
        .expect("attente de la fin du processus npu")
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Écrit un scope `$XDG_CONFIG_HOME/npu` dont `backends/ovms.toml` est un
/// TOML illisible (échec de PARSING, donc fatal même masqué — cf.
/// IMPLEMENTATION.md phase 2 : contrairement à une commande, un backend
/// cassé n'a pas d'identité connaissable avant d'être parsé).
fn write_broken_scope(xdg_root: &Path) {
    write(
        xdg_root,
        "npu/backends/ovms.toml",
        "ceci n'est pas du TOML valide { { {\n",
    );
}

/// Écrit un scope `$XDG_CONFIG_HOME/npu` SAIN : un backend `ovms` pointant
/// sur `base_url`, un modèle `qwen-fast`, et la commande `commit-message`
/// (format texte, sans schéma — la vérification (e) de `doctor` ne doit rien
/// produire pour elle).
fn write_healthy_scope(xdg_root: &Path, base_url: &str) {
    write(
        xdg_root,
        "npu/backends/ovms.toml",
        &format!(
            r#"
            id = "ovms"
            base_url = "{base_url}"
            type = "openai-compatible"

            [operations.chat]
            method = "POST"
            path = "/v3/chat/completions"
            "#
        ),
    );
    write(
        xdg_root,
        "npu/models/qwen-fast.toml",
        r#"
        id = "qwen-fast"
        backend = "ovms"
        operation = "chat"
        model = "qwen-fast-underlying"
        "#,
    );
    write(
        xdg_root,
        "npu/commands/commit-message.md",
        "+++\ndescription = \"Generate a commit message\"\nmodel = \"qwen-fast\"\n+++\n\
         {{ input }}\n",
    );
}

// -- (a)/(b)/(c) : configuration cassée -------------------------------------

/// (a) Une configuration CASSÉE laisse `--help` utilisable (mode dégradé,
/// point 1 du contrat partagé) : code 0, stdout liste les trois built-ins,
/// stderr signale l'échec de chargement.
#[test]
fn broken_config_help_still_works_and_lists_builtins_with_stderr_signal() {
    let xdg = fixture_dir("broken-help-xdg");
    let cwd = fixture_dir("broken-help-cwd");
    write_broken_scope(&xdg);

    let output = run_npu(&cwd, &xdg, &["--help"]);

    assert!(
        output.status.success(),
        "PREUVE (a) : npu --help doit réussir (exit 0) malgré une configuration cassée, \
         obtenu code {:?} ; stderr : {}",
        output.status.code(),
        stderr_of(&output)
    );
    let stdout = stdout_of(&output);
    assert!(
        stdout.contains("doctor"),
        "stdout de --help doit lister « doctor », obtenu : {stdout}"
    );
    assert!(
        stdout.contains("models"),
        "stdout de --help doit lister « models », obtenu : {stdout}"
    );
    assert!(
        stdout.contains("describe"),
        "stdout de --help doit lister « describe », obtenu : {stdout}"
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.to_lowercase().contains("invalide") && stderr.contains("doctor"),
        "PREUVE (a) : stderr doit signaler une configuration invalide et renvoyer vers « npu \
         doctor », obtenu : {stderr}"
    );
}

/// (b) Même configuration cassée : `npu doctor` sort en code 2 et son
/// rapport, sur STDOUT, décrit l'erreur de chargement.
#[test]
fn broken_config_doctor_reports_load_error_on_stdout_with_exit_code_two() {
    let xdg = fixture_dir("broken-doctor-xdg");
    let cwd = fixture_dir("broken-doctor-cwd");
    write_broken_scope(&xdg);

    let output = run_npu(&cwd, &xdg, &["doctor"]);

    assert_eq!(
        output.status.code(),
        Some(2),
        "PREUVE (b) : npu doctor doit sortir en code 2 sur une configuration cassée, stderr : {}",
        stderr_of(&output)
    );
    let stdout = stdout_of(&output);
    assert!(
        stdout.contains('✗') && stdout.to_lowercase().contains("configuration"),
        "PREUVE (b) : le rapport de doctor (sur stdout) doit décrire l'échec de la vérification \
         de configuration, obtenu : {stdout}"
    );
    assert!(
        stdout.contains("ovms.toml"),
        "PREUVE (b) : le rapport doit nommer le fichier fautif, obtenu : {stdout}"
    );
}

/// (c) Même configuration cassée : une invocation quelconque sort en code 2.
///
/// Deux mécanismes DISTINCTS partagent ce code, et ce test les couvre tous
/// les deux séparément plutôt que de les confondre :
/// - une commande métier (`commit-message`) n'existe même pas dans l'arbre
///   `clap` en mode dégradé (`build_cli(&[])`, `write_broken_scope` ne
///   déclare d'ailleurs aucune commande) : `clap` la rejette lui-même comme
///   sous-commande inconnue (même chemin que `tests/
///   clap_error_stdout_purity.rs`), AVANT que `run()` n'atteigne `loaded?` —
///   ce n'est PAS une propagation de l'erreur de chargement, seulement un
///   code de sortie qui coïncide ;
/// - `models` et `describe`, elles, restent TOUJOURS dans l'arbre `clap`
///   (ajoutées inconditionnellement par `add_builtins`) : leur code 2
///   traverse bien `loaded?`, donc porte l'erreur de chargement CONSERVÉE —
///   vérifié ici en exigeant que stderr nomme le fichier fautif, pas
///   seulement le code de sortie (point 1 du contrat partagé).
#[test]
fn broken_config_any_other_invocation_exits_with_code_two() {
    let xdg = fixture_dir("broken-any-xdg");
    let cwd = fixture_dir("broken-any-cwd");
    write_broken_scope(&xdg);

    // Mécanisme 1 : rejet par `clap` lui-même (aucune commande métier dans
    // l'arbre en mode dégradé), pas une propagation de `loaded?`.
    let business = run_npu(&cwd, &xdg, &["commit-message"]);
    assert_eq!(
        business.status.code(),
        Some(2),
        "PREUVE (c) : une sous-commande absente de l'arbre en mode dégradé doit sortir en code \
         2 (rejet `clap`), stderr : {}",
        stderr_of(&business)
    );
    assert!(
        business.stdout.is_empty(),
        "rien ne doit être écrit sur stdout en cas d'échec, obtenu : {}",
        stdout_of(&business)
    );

    // Mécanisme 2 : `models` traverse `loaded?` (toujours dans l'arbre) —
    // stderr doit porter l'erreur de chargement CONSERVÉE, pas un message
    // générique, sans quoi ce test ne distinguerait pas ce chemin du
    // mécanisme 1 ci-dessus.
    let models = run_npu(&cwd, &xdg, &["models"]);
    assert_eq!(
        models.status.code(),
        Some(2),
        "PREUVE (c) : npu models doit propager l'erreur de chargement (via loaded?), code 2, \
         stderr : {}",
        stderr_of(&models)
    );
    assert!(
        stderr_of(&models).contains("ovms.toml"),
        "PREUVE (c) : stderr de npu models doit nommer le fichier de configuration fautif \
         (preuve que c'est bien l'erreur de chargement CONSERVÉE qui est propagée), obtenu : {}",
        stderr_of(&models)
    );

    // Même preuve que `models`, pour `describe`.
    let describe = run_npu(&cwd, &xdg, &["describe", "commit-message"]);
    assert_eq!(
        describe.status.code(),
        Some(2),
        "PREUVE (c) : npu describe doit propager l'erreur de chargement (via loaded?), code 2, \
         stderr : {}",
        stderr_of(&describe)
    );
    assert!(
        stderr_of(&describe).contains("ovms.toml"),
        "PREUVE (c) : stderr de npu describe doit nommer le fichier de configuration fautif, \
         obtenu : {}",
        stderr_of(&describe)
    );
}

// -- (d)/(e)/(f) : configuration saine --------------------------------------

/// Lie un `TcpListener` éphémère puis le ferme aussitôt, pour obtenir un port
/// réellement fermé sans jamais coder de numéro en dur ni toucher au port
/// 8000 (celui de la fixture `tests/output_contract_e2e.rs`) — même idiome
/// que `builtin::tests::tcp_probe_fails_against_a_closed_port`.
fn closed_port_base_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind du listener éphémère");
    let addr = listener.local_addr().expect("adresse locale du listener");
    drop(listener); // ferme immédiatement : plus personne n'écoute ici.
    format!("http://{addr}")
}

/// (d) Configuration SAINE mais backend éteint (port fermé) : `npu doctor`
/// sort en code 3 (jamais 2 : la configuration elle-même est valide), et le
/// rapport montre les vérifications de configuration en succès et la
/// joignabilité en échec.
#[test]
fn healthy_config_with_dead_backend_doctor_exits_three_with_reachability_failure_only() {
    let xdg = fixture_dir("healthy-dead-backend-xdg");
    let cwd = fixture_dir("healthy-dead-backend-cwd");
    write_healthy_scope(&xdg, &closed_port_base_url());

    let output = run_npu(&cwd, &xdg, &["doctor"]);

    assert_eq!(
        output.status.code(),
        Some(3),
        "PREUVE (d) : npu doctor doit sortir en code 3 (backend injoignable, config saine), \
         stderr : {}",
        stderr_of(&output)
    );
    let stdout = stdout_of(&output);
    assert!(
        stdout.contains("✓ configuration chargée"),
        "PREUVE (d) : la vérification (a) doit être en succès, obtenu : {stdout}"
    );
    assert!(
        stdout.contains("✓ modèle « qwen-fast »"),
        "PREUVE (d) : la vérification (c) (modèle) doit être en succès, obtenu : {stdout}"
    );
    assert!(
        stdout.contains("✓ commande « commit-message » : modèle"),
        "PREUVE (d) : la vérification (d) (commande) doit être en succès, obtenu : {stdout}"
    );
    assert!(
        stdout.contains('✗') && stdout.contains("joignable"),
        "PREUVE (d) : la vérification de joignabilité (b) doit être en échec, obtenu : {stdout}"
    );
}

/// (e) Configuration saine, `npu models` : code 0, stdout contient
/// `qwen-fast`, `ovms` et `chat` (colonnes NAME/BACKEND/OPERATION, §16).
#[test]
fn healthy_config_models_lists_configured_model_with_its_backend_and_operation() {
    let xdg = fixture_dir("healthy-models-xdg");
    let cwd = fixture_dir("healthy-models-cwd");
    // Backend jamais contacté par `models` : une URL syntaxiquement valide
    // mais non résolue à un service réel suffit.
    write_healthy_scope(&xdg, "http://127.0.0.1:1");

    let output = run_npu(&cwd, &xdg, &["models"]);

    assert!(
        output.status.success(),
        "PREUVE (e) : npu models doit réussir (exit 0), stderr : {}",
        stderr_of(&output)
    );
    let stdout = stdout_of(&output);
    assert!(stdout.contains("qwen-fast"), "obtenu : {stdout}");
    assert!(stdout.contains("ovms"), "obtenu : {stdout}");
    assert!(stdout.contains("chat"), "obtenu : {stdout}");
}

/// (f) Configuration saine, `npu describe commit-message` : code 0, stdout
/// est du JSON parsable décrivant la commande.
#[test]
fn healthy_config_describe_produces_parsable_json_for_a_known_command() {
    let xdg = fixture_dir("healthy-describe-xdg");
    let cwd = fixture_dir("healthy-describe-cwd");
    write_healthy_scope(&xdg, "http://127.0.0.1:1");

    let output = run_npu(&cwd, &xdg, &["describe", "commit-message"]);

    assert!(
        output.status.success(),
        "PREUVE (f) : npu describe commit-message doit réussir (exit 0), stderr : {}",
        stderr_of(&output)
    );
    let stdout = stdout_of(&output);
    let value: serde_json::Value = serde_json::from_str(stdout.trim())
        .expect("PREUVE (f) : stdout doit être du JSON parsable");
    assert_eq!(value["name"], "commit-message");
    assert_eq!(value["model"], "qwen-fast");
}

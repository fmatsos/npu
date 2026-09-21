//! Integration tests.
//!
//! - `discovers_commit_message_fixture`: checks that `command::discover`
//!   finds the `.npu/commands/commit-message.md` fixture with the correct model
//!   and the correct input mode.
//! - `layered_scopes_local_wins_over_general` (phase 2): builds two
//!   temporary scope roots under `target/` (a "general" one and a
//!   "local" one, both distinct from the versioned `.npu/` fixture) and
//!   checks that `discover_scopes` and `load_scopes` correctly keep the
//!   local version of a command AND a model defined in both.
//!
//! None of these tests call the network or touch real environment
//! variables: `discover_scopes`/`load_scopes` are pure functions
//! over an explicitly passed list of roots, never `scope::roots()`.
#![allow(clippy::expect_used)] // allowed in tests (see Cargo.toml [lints.clippy]).

use npu::command::{self, InputMode};
use npu::config;
use npu::prompt;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};

#[test]
fn discovers_commit_message_fixture() {
    let root = std::path::Path::new(".npu");
    let commands = command::discover(root).expect("command discovery must succeed");

    let commit_message = commands
        .iter()
        .find(|spec| spec.path == vec!["commit-message".to_string()])
        .expect("commit-message must be discovered");

    assert_eq!(commit_message.model, "qwen-fast");
    assert!(matches!(commit_message.input, InputMode::Stdin));
}

/// Discovers the versioned `.npu/commands/translate.md` fixture (phase 3,
/// npu-cli-spec.md §11): checks that the `language` argument is declared with
/// the correct short letter and the correct `required` flag, and that its prompt
/// (which references `{{ args.language }}`) correctly passes the static
/// placeholder validation (§12). Does not call the network: `command::discover`
/// only reads and parses local files.
#[test]
fn discovers_translate_fixture_with_declared_language_arg() {
    let root = std::path::Path::new(".npu");
    let commands = command::discover(root).expect("command discovery must succeed");

    let translate = commands
        .iter()
        .find(|spec| spec.path == vec!["translate".to_string()])
        .expect("translate must be discovered");

    let language = translate
        .args
        .get("language")
        .expect("the 'language' argument must be declared");
    assert_eq!(language.short, Some('l'));
    assert!(language.required, "'language' must be required = true");

    let declared: BTreeSet<String> = translate.args.keys().cloned().collect();
    prompt::validate(&translate.prompt, &declared)
        .expect("the translate prompt must pass static placeholder validation");
}

/// Discovers the versioned `.npu/commands/classify.md` fixture (phase 4,
/// npu-cli-spec.md §6/§15): checks that the command correctly declares
/// `format = "json"` AND that the resolved schema path (`schemas/
/// classification.json`, relative to the `.npu/` scope root) actually
/// EXISTS on disk — the exact debt this phase repays
/// (`[output]` read and then honored, not just accepted). Does not call
/// the network: `command::discover` only reads and parses local
/// files.
#[test]
fn discovers_classify_fixture_with_json_format_and_existing_schema() {
    let root = std::path::Path::new(".npu");
    let commands = command::discover(root).expect("command discovery must succeed");

    let classify = commands
        .iter()
        .find(|spec| spec.path == vec!["classify".to_string()])
        .expect("classify must be discovered");

    assert_eq!(
        classify.output.format,
        npu::output::Format::Json,
        "classify must declare format = \"json\""
    );
    let schema_path = classify
        .output
        .schema
        .as_deref()
        .expect("classify must declare a schema path");
    assert!(
        schema_path.is_file(),
        "the resolved schema path must actually exist on disk, got: {}",
        schema_path.display()
    );
}

/// Creates a unique fixture directory under `target/`, to avoid polluting the
/// repo or colliding between tests run in parallel (same idiom
/// as `config::tests::fixture_dir` / `command::tests::fixture_dir`).
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
        std::fs::create_dir_all(parent).expect("failed to create parent directory");
    }
    std::fs::write(path, contents).expect("failed to write fixture");
}

#[test]
fn layered_scopes_local_wins_over_general() {
    let general = fixture_dir("general");
    let local = fixture_dir("local");

    // General scope: one command and one model (with its backend).
    write(
        &general,
        "commands/classify.md",
        "+++\ndescription = \"Classify (general)\"\nmodel = \"qwen-fast\"\n+++\ngeneral prompt\n",
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

    // Local scope: redefines the same command and the same model.
    write(
        &local,
        "commands/classify.md",
        "+++\ndescription = \"Classify (local)\"\nmodel = \"qwen-fast\"\n+++\nlocal prompt\n",
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

    // Order general -> local, exactly what `scope::roots()` produces.
    let roots = vec![general, local];

    let specs = command::discover_scopes(&roots).expect("layered discovery must succeed");
    let classify = specs
        .iter()
        .find(|spec| spec.path == vec!["classify".to_string()])
        .expect("classify must be discovered");
    assert_eq!(classify.prompt, "local prompt");
    assert!(matches!(classify.input, InputMode::Stdin));

    let config = config::load_scopes(&roots).expect("layered loading must succeed");
    let (model, backend) = config
        .resolve("qwen-fast")
        .expect("qwen-fast must resolve, with the backend inherited from the general scope");
    assert_eq!(
        model.model, "qwen-local",
        "the local model must fully override the general model"
    );
    assert_eq!(backend.id, "ovms");
}

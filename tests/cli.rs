//! Test d'intégration : vérifie que `command::discover` retrouve la fixture
//! `.npu/commands/commit-message.md` avec le bon modèle et le bon mode
//! d'entrée. N'appelle pas le réseau.
#![allow(clippy::expect_used)] // toléré dans les tests (cf. Cargo.toml [lints.clippy]).

use npu::command::{self, InputMode};

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

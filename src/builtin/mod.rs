//! CLI built-ins: `doctor`, `models`, `describe`, `version`, `update`, and the TCP
//! reachability probe they share.
//!
//! This module NEVER writes to the console itself: [`doctor::doctor`] returns
//! a report ([`Check`]) that the caller (`cli::builtins::doctor`) formats
//! with [`doctor::format_doctor`] before writing it to stdout (contract rule: stdout
//! is reserved for the RESULT of a built-in, which IS its report). This is
//! what makes [`doctor::doctor`] fully testable without touching disk beyond
//! check (e), nor the network: the reachability probe (`probe`) is
//! injected by the caller rather than called directly, exactly like
//! `prompt::render`/`prompt::preflight` inject their environment
//! variable resolver (cf. `exec::execute_business_command`).
//!
//! **Deliberate omission: "NPU available".** This CLI is deliberately
//! agnostic of the inference runtime (the core only knows named backend
//! operations, never an NPU in the hardware sense): it therefore has NO way to
//! check the presence or availability of an NPU, unlike the
//! reachability of a backend (a TCP connection) or the validity of a
//! schema (a disk file read). Showing a checkmark for a check that
//! wasn't actually performed would be a report that lies, which is worse
//! than no report: [`doctor::doctor`] therefore NEVER produces a [`Check`] for
//! this line. This is not an oversight.
//!
//! Split into submodules by concern: `doctor` (the `doctor`/`config check`
//! report), `describe` (the JSON description of a built-in or configured
//! command), [`models`] (the `config models` table), [`lifecycle`]
//! (`backend serve/stop/logs/status`), and [`net`] (the TCP reachability
//! probe `doctor` injects). This module keeps the vocabulary shared by all
//! of them — [`Status`], [`CheckKind`], [`Check`], [`RESERVED`] — and
//! re-exports every submodule item so `crate::builtin::X` paths are
//! unaffected by the split.

mod describe;
mod doctor;
mod lifecycle;
mod models;
mod net;

pub use describe::{describe, describe_builtin};
pub use doctor::{Probes, doctor, doctor_exit_code, format_doctor};
pub use lifecycle::{logs, serve, status, stop};
pub use models::format_models;
pub use net::tcp_probe;

pub(crate) use net::parse_host_port;

#[cfg(test)]
pub(crate) use lifecycle::NOT_STARTED;

/// Outcome of a [`doctor::doctor`] check.
#[derive(Debug)]
pub enum Status {
    /// The check succeeded.
    Ok,
    /// The check failed, with an actionable message.
    Failed(String),
}

/// Category of a [`Check`], in the sense of the exit code from [`doctor::doctor_exit_code`]:
/// it is this value, and
/// NEVER the text of [`Check::label`], that distinguishes a
/// configuration failure ("fix your files") from a reachability failure
/// ("start your runtime") for a calling agent. A label is
/// display — it can be reworded, translated, or given a new
/// suffix without notice; the category is a machine contract and must
/// survive that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckKind {
    /// Checks (a), (c), (d), (e): configuration, models, commands.
    Config,
    /// Check (b): TCP reachability of a backend.
    Reachability,
}

/// One line of the [`doctor::doctor`] report.
#[derive(Debug)]
pub struct Check {
    /// Category of this check, used by [`doctor::doctor_exit_code`].
    pub kind: CheckKind,
    /// What was checked (e.g. `backend "ovms" reachable`).
    pub label: String,
    /// The result of this check.
    pub status: Status,
}

/// The built-in names exposed by this CLI, plus `help`, reserved by
/// `clap` itself (every `clap::Command` gets an automatic `-h`/`--help`
/// flag). `command.rs` (`reject_reserved_path`) rejects at load time
/// any command file whose FIRST path segment matches one of these
/// values: without this
/// rejection, `commands/doctor.md` would be silently shadowed by (or
/// would shadow) the `doctor` built-in built in `cli::builtins`.
pub const RESERVED: &[&str] = &[
    "backend", "config", "doctor", "describe", "update", "help", "model",
];

#[cfg(test)]
#[allow(clippy::expect_used)] // allowed in tests (cf. Cargo.toml [lints.clippy]).
#[allow(clippy::panic)] // a stub that must never be called says so by panicking.
mod tests {
    use super::*;

    fn backend(id: &str, base_url: &str, operations: &[&str]) -> crate::config::Backend {
        crate::config::Backend {
            id: id.to_string(),
            base_url: base_url.to_string(),
            kind: "openai-compatible".to_string(),
            operations: operations
                .iter()
                .map(|op| {
                    (
                        (*op).to_string(),
                        crate::config::Operation {
                            method: "POST".to_string(),
                            path: format!("/{op}"),
                        },
                    )
                })
                .collect(),
            port: None,
            runtime: None,
            docker: None,
            timeouts: None,
            structured_output: false,
            headers: std::collections::BTreeMap::new(),
            source: std::path::PathBuf::new(),
        }
    }

    /// Same as [`backend`], plus a Docker `[runtime]`: `options`, `image` and
    /// `args` are deliberately distinguishable so that a test can assert on
    /// their ORDER in the built command line.
    fn containerized_backend(id: &str) -> crate::config::Backend {
        let mut backend = backend(id, "http://127.0.0.1:8000", &["chat"]);
        backend.runtime = Some(crate::config::Runtime::Docker(crate::config::Docker {
            image: "example/server:latest".to_string(),
            options: vec![
                "-p".to_string(),
                "8000:8000".to_string(),
                "-v".to_string(),
                "{{ env.NPU_TEST_MODELS }}:/models".to_string(),
            ],
            args: vec!["--source_model".to_string(), "{{ args.model }}".to_string()],
        }));
        backend
    }

    /// The most common way anyone hits `serve` twice. Diagnosing it as a
    /// port conflict would send the user to edit a `port` key that is
    /// perfectly correct, so the container is checked FIRST.
    #[test]
    fn serve_on_an_already_served_backend_points_at_the_container_not_the_port() {
        let config = containerized_config();

        let err = serve(
            &config,
            "qwen",
            &test_env,
            &|args: &[String]| {
                assert_eq!(args.first().map(String::as_str), Some("ps"));
                Ok("npu-ovms\n".to_string())
            },
            &test_host(),
        )
        .expect_err("an already-served backend must fail");

        assert_eq!(err.exit_code(), 3);
        let message = err.to_string();
        assert!(message.contains("npu-ovms"), "got: {message}");
        assert!(message.contains("qwen"), "got: {message}");
    }

    #[test]
    fn serve_reports_a_fixed_port_already_in_use_naming_the_backend() {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("binding the occupying listener");
        let port = listener.local_addr().expect("local address").port();

        let mut config = crate::config::Config::default();
        let mut backend = backend("ovms", &format!("http://127.0.0.1:{port}"), &["chat"]);
        backend.port = Some(crate::config::Port::Fixed(port));
        backend.runtime = Some(crate::config::Runtime::Docker(crate::config::Docker {
            image: "img".to_string(),
            options: vec![],
            args: vec![],
        }));
        config.backends.insert("ovms".to_string(), backend);
        config
            .models
            .insert("m".to_string(), model("m", "ovms", "chat"));

        let err = serve(
            &config,
            "m",
            &|_| None,
            &|args: &[String]| {
                assert_eq!(
                    args.first().map(String::as_str),
                    Some("ps"),
                    "only the container probe may run when the port is already taken"
                );
                Ok(String::new())
            },
            &test_host(),
        )
        .expect_err("an occupied fixed port must fail");

        assert_eq!(err.exit_code(), 3);
        let message = err.to_string();
        assert!(message.contains("ovms"), "got: {message}");
        assert!(message.contains(&port.to_string()), "got: {message}");
    }

    /// A runner no `doctor` test needs: every fixture uses a fixed port, so
    /// `resolve_base_url` returns before touching it. Calling it is the bug.
    fn unused_runner(_args: &[String]) -> crate::Result<String> {
        panic!("a fixed-port backend must never ask Docker for its port")
    }

    fn model(id: &str, backend: &str, operation: &str) -> crate::config::Model {
        crate::config::Model {
            id: id.to_string(),
            backend: backend.to_string(),
            operation: operation.to_string(),
            model: format!("{id}-underlying"),
            fallback: None,
            generation: crate::config::Generation::default(),
            source: std::path::PathBuf::new(),
        }
    }

    fn command_spec(path: &[&str], model: &str) -> crate::command::CommandSpec {
        crate::command::CommandSpec {
            path: path.iter().map(ToString::to_string).collect(),
            description: String::new(),
            model: model.to_string(),
            input: crate::command::InputMode::Stdin,
            prompt: "{{ input }}".to_string(),
            args: std::collections::BTreeMap::new(),
            output: crate::output::OutputSpec::default(),
            schemas: std::collections::BTreeMap::new(),
            system: None,
            examples: Vec::new(),
            generation: None,
            file: std::path::PathBuf::new(),
        }
    }

    // `unnecessary_wraps`: these two functions are stub probes with a
    // FIXED signature (`&dyn Fn(&str) -> Result<(), String>`, cf.
    // `doctor`); they cannot be simplified without breaking that
    // signature.
    #[allow(clippy::unnecessary_wraps)]
    fn always_ok(_base_url: &str) -> Result<(), String> {
        Ok(())
    }

    fn always_fails(_base_url: &str) -> Result<(), String> {
        Err("connection refused".to_string())
    }

    // Same reasoning as `always_ok`/`always_fails`, for the container
    // runtime probe injected into `doctor` (signature `&dyn Fn() ->
    // Result<(), String>`): no test of this module ever needs Docker.
    #[allow(clippy::unnecessary_wraps)]
    fn container_ok() -> Result<(), String> {
        Ok(())
    }

    fn container_fails() -> Result<(), String> {
        Err("cannot run \"docker\"".to_string())
    }

    // Same shape again for the runtime-command probe injected into
    // `doctor` (signature `&dyn Fn(&str) -> Result<(), String>`): no test
    // of this module ever needs an inference server installed.
    #[allow(clippy::unnecessary_wraps)]
    fn command_ok(_command: &str) -> Result<(), String> {
        Ok(())
    }

    fn command_fails(_command: &str) -> Result<(), String> {
        Err("not found on PATH".to_string())
    }

    /// The three stub probes as one bundle. The runner is always the one
    /// that must never be called: every `doctor` fixture here uses a fixed
    /// port, so `resolve_base_url` returns before touching it.
    fn probes<'a>(
        backend: &'a dyn Fn(&str) -> Result<(), String>,
        container: &'a dyn Fn() -> Result<(), String>,
        command: &'a dyn Fn(&str) -> Result<(), String>,
    ) -> Probes<'a> {
        Probes {
            backend,
            container,
            command,
            runner: &unused_runner,
        }
    }

    // The process runtime's injected outside world, for the tests that do
    // not exercise it: every closure panics, so a Docker test that somehow
    // reached the process family would say so instead of passing quietly.
    // The state environment names no home for the same reason.
    fn unused_env(_name: &str) -> Option<String> {
        panic!("the process runtime's environment must not be read here")
    }

    fn unused_inspect(_pid: u32) -> Option<crate::runtime::process::ProcessFacts> {
        panic!("no process must be inspected here")
    }

    fn unused_signal(_pid: u32, _signal: crate::runtime::process::Signal) -> bool {
        panic!("no process must be signalled here")
    }

    fn unused_probe(_base_url: &str) -> Result<(), String> {
        panic!("the process runtime's probe must not run here")
    }

    fn unused_sink(_bytes: &[u8]) -> crate::Result<()> {
        panic!("a container's logs go through the streamer, never through this sink")
    }

    fn test_host() -> crate::runtime::process::Host<'static> {
        crate::runtime::process::Host {
            env: &unused_env,
            state: crate::runtime::state::StateEnv {
                xdg_state_home: None,
                home: None,
            },
            inspect: &unused_inspect,
            signal: &unused_signal,
            probe: &unused_probe,
        }
    }

    // -- doctor: nominal scenario -----------------------------------------

    #[test]
    fn doctor_all_checks_passing_yields_no_failure_and_exit_code_zero() {
        let mut config = crate::config::Config::default();
        config.backends.insert(
            "ovms".to_string(),
            backend("ovms", "http://127.0.0.1:8000", &["chat"]),
        );
        config
            .models
            .insert("qwen-fast".to_string(), model("qwen-fast", "ovms", "chat"));
        let commands = vec![command_spec(&["classify"], "qwen-fast")];

        let checks = doctor(
            Some(&config),
            Some(&commands),
            None,
            &probes(&always_ok, &container_ok, &command_ok),
        );

        assert!(
            checks.iter().all(|c| matches!(c.status, Status::Ok)),
            "got: {checks:?}"
        );
        assert_eq!(doctor_exit_code(&checks), 0);
    }

    // -- (a) configuration loaded ------------------------------------------

    #[test]
    fn doctor_load_error_fails_configuration_check_only_and_exit_code_is_two() {
        let err = crate::Error::config("broken command file");

        let checks = doctor(
            None,
            None,
            Some(&err),
            &probes(&always_ok, &container_ok, &command_ok),
        );

        assert_eq!(
            checks.len(),
            1,
            "config/commands absent: only (a) must be produced, got: {checks:?}"
        );
        assert!(matches!(&checks[0].status, Status::Failed(_)));
        assert_eq!(doctor_exit_code(&checks), 2);
    }

    // -- (c) models ----------------------------------------------------------

    #[test]
    fn doctor_model_referencing_unknown_backend_fails_check_c_and_exit_code_is_two() {
        let mut config = crate::config::Config::default();
        config
            .models
            .insert("gpt".to_string(), model("gpt", "does-not-exist", "chat"));

        let checks = doctor(
            Some(&config),
            Some(&[]),
            None,
            &probes(&always_ok, &container_ok, &command_ok),
        );

        let model_check = checks
            .iter()
            .find(|c| c.label.contains("gpt"))
            .expect("a check for model \"gpt\" must exist");
        assert!(matches!(&model_check.status, Status::Failed(_)));
        assert_eq!(doctor_exit_code(&checks), 2);
    }

    #[test]
    fn doctor_model_referencing_unexposed_operation_fails_check_c() {
        let mut config = crate::config::Config::default();
        config.backends.insert(
            "ovms".to_string(),
            backend("ovms", "http://127.0.0.1:8000", &["chat"]),
        );
        config.models.insert(
            "whisper".to_string(),
            model("whisper", "ovms", "audio_transcriptions"),
        );

        let checks = doctor(
            Some(&config),
            Some(&[]),
            None,
            &probes(&always_ok, &container_ok, &command_ok),
        );

        let model_check = checks
            .iter()
            .find(|c| c.label.contains("whisper"))
            .expect("a check for model \"whisper\" must exist");
        assert!(matches!(
            &model_check.status,
            Status::Failed(message) if message.contains("audio_transcriptions")
        ));
        assert_eq!(doctor_exit_code(&checks), 2);
    }

    // -- (d) commands / model ------------------------------------------------

    #[test]
    fn doctor_command_referencing_unknown_model_fails_check_d() {
        let config = crate::config::Config::default();
        let commands = vec![command_spec(&["classify"], "does-not-exist")];

        let checks = doctor(
            Some(&config),
            Some(&commands),
            None,
            &probes(&always_ok, &container_ok, &command_ok),
        );

        let command_check = checks
            .iter()
            .find(|c| c.label.contains("classify") && c.label.contains("model"))
            .expect("a check (d) for \"classify\" must exist");
        assert!(matches!(&command_check.status, Status::Failed(_)));
        assert_eq!(doctor_exit_code(&checks), 2);
    }

    // -- (e) commands / output schema ---------------------------------------

    #[test]
    fn doctor_command_with_missing_schema_file_fails_check_e() {
        let mut spec = command_spec(&["classify"], "qwen-fast");
        spec.output = crate::output::OutputSpec {
            format: crate::output::Format::Json,
            schema: Some(std::path::PathBuf::from(
                "/does/not/exist/schema-not-found.json",
            )),
            max_lines: None,
            allow_truncated: false,
            strip_reasoning: false,
        };
        let config = crate::config::Config::default();

        let checks = doctor(
            Some(&config),
            Some(&[spec]),
            None,
            &probes(&always_ok, &container_ok, &command_ok),
        );

        let schema_check = checks
            .iter()
            .find(|c| c.label.contains("schema"))
            .expect("a check (e) must exist");
        assert!(matches!(&schema_check.status, Status::Failed(_)));
    }

    #[test]
    fn doctor_command_without_schema_produces_no_check_e() {
        let spec = command_spec(&["commit-message"], "qwen-fast"); // text format by default
        let config = crate::config::Config::default();

        let checks = doctor(
            Some(&config),
            Some(&[spec]),
            None,
            &probes(&always_ok, &container_ok, &command_ok),
        );

        assert!(
            !checks.iter().any(|c| c.label.contains("schema")),
            "no check (e) must be produced in the absence of a schema, got: {checks:?}"
        );
    }

    // -- exit code: configuration takes priority over reachability -----

    #[test]
    fn doctor_reachability_failure_alone_yields_exit_code_three() {
        let mut config = crate::config::Config::default();
        config.backends.insert(
            "ovms".to_string(),
            backend("ovms", "http://127.0.0.1:8000", &["chat"]),
        );

        let checks = doctor(
            Some(&config),
            Some(&[]),
            None,
            &probes(&always_fails, &container_ok, &command_ok),
        );

        assert_eq!(doctor_exit_code(&checks), 3);
    }

    /// Regression: classification must come from [`CheckKind`], never
    /// from the label text. A reachability label deliberately reworded,
    /// without the word "reachable" nor the historical suffix, must
    /// still produce exit code 3 — text-based classification silently
    /// reclassified this case as a configuration failure (code 2).
    #[test]
    fn doctor_exit_code_uses_kind_not_label_text_for_reachability_failure() {
        let checks = vec![Check {
            kind: CheckKind::Reachability,
            label: "backend \"ovms\" status".to_string(),
            status: Status::Failed("connection refused".to_string()),
        }];

        assert_eq!(doctor_exit_code(&checks), 3);
    }

    #[test]
    fn doctor_config_failure_takes_priority_over_reachability_failure() {
        let mut config = crate::config::Config::default();
        config.backends.insert(
            "ovms".to_string(),
            backend("ovms", "http://127.0.0.1:8000", &["chat"]),
        );
        // (c) fails: the model references a nonexistent backend.
        config.models.insert(
            "orphan".to_string(),
            model("orphan", "does-not-exist", "chat"),
        );

        // (b) also fails: the probe fails for every backend queried.
        let checks = doctor(
            Some(&config),
            Some(&[]),
            None,
            &probes(&always_fails, &container_ok, &command_ok),
        );

        let reachability_failed = checks
            .iter()
            .any(|c| c.kind == CheckKind::Reachability && matches!(c.status, Status::Failed(_)));
        let config_check_failed = checks
            .iter()
            .any(|c| c.label.contains("orphan") && matches!(c.status, Status::Failed(_)));
        assert!(
            reachability_failed,
            "precondition: the probe must have failed"
        );
        assert!(
            config_check_failed,
            "precondition: check (c) must have failed"
        );

        assert_eq!(
            doctor_exit_code(&checks),
            2,
            "configuration must take priority over reachability"
        );
    }

    // -- format_doctor -----------------------------------------------------------

    #[test]
    fn format_doctor_uses_checkmark_for_ok_and_cross_with_message_for_failed() {
        let checks = vec![
            Check {
                kind: CheckKind::Config,
                label: "a".to_string(),
                status: Status::Ok,
            },
            Check {
                kind: CheckKind::Config,
                label: "b".to_string(),
                status: Status::Failed("boom".to_string()),
            },
        ];

        // What a pipe receives: `anstream` strips the colours on the way out.
        let plain = anstream::adapter::strip_str(&format_doctor(&checks)).to_string();
        assert_eq!(plain, "✓ a\n✗ b: boom");
    }

    // -- format_models -------------------------------------------------------------

    #[test]
    fn format_models_aligns_backend_column_across_rows_of_differing_name_length() {
        let mut config = crate::config::Config::default();
        config
            .models
            .insert("a".to_string(), model("a", "ovms", "chat"));
        config.models.insert(
            "much-longer-model-name".to_string(),
            model("much-longer-model-name", "ovms", "chat"),
        );

        let out = format_models(&config);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3);

        let header_backend_at = lines[0].find("BACKEND").expect("BACKEND header");
        let row_a_backend_at = lines[1].find("ovms").expect("row \"a\"");
        let row_long_backend_at = lines[2].find("ovms").expect("row \"much-longer…\"");

        assert_eq!(header_backend_at, row_a_backend_at);
        assert_eq!(header_backend_at, row_long_backend_at);
    }

    #[test]
    fn format_models_sorts_deterministically_by_name() {
        let mut config = crate::config::Config::default();
        config
            .models
            .insert("zebra".to_string(), model("zebra", "ovms", "chat"));
        config
            .models
            .insert("alpha".to_string(), model("alpha", "ovms", "chat"));

        let out = format_models(&config);
        let lines: Vec<&str> = out.lines().collect();

        let alpha_at = lines
            .iter()
            .position(|l| l.starts_with("alpha"))
            .expect("\"alpha\" must be present");
        let zebra_at = lines
            .iter()
            .position(|l| l.starts_with("zebra"))
            .expect("\"zebra\" must be present");
        assert!(alpha_at < zebra_at);
    }

    #[test]
    fn format_models_with_no_models_prints_only_the_header_without_panicking() {
        let config = crate::config::Config::default();

        let out = format_models(&config);

        assert_eq!(out, "NAME  BACKEND  OPERATION  FALLBACK");
    }

    // -- describe --------------------------------------------------------------

    fn sample_translate_spec() -> crate::command::CommandSpec {
        let mut args = std::collections::BTreeMap::new();
        args.insert(
            "language".to_string(),
            crate::command::ArgSpec {
                short: Some('l'),
                required: true,
                description: "Target language".to_string(),
            },
        );
        crate::command::CommandSpec {
            path: vec!["translate".to_string()],
            description: "Translate input text".to_string(),
            model: "qwen-fast".to_string(),
            input: crate::command::InputMode::StdinOrFile,
            prompt: "Translate {{ input }} to {{ args.language }}".to_string(),
            args,
            output: crate::output::OutputSpec {
                format: crate::output::Format::Json,
                schema: Some(std::path::PathBuf::from("schemas/translation.json")),
                max_lines: None,
                allow_truncated: false,
                strip_reasoning: false,
            },
            schemas: std::collections::BTreeMap::new(),
            system: None,
            examples: Vec::new(),
            generation: None,
            file: std::path::PathBuf::from(".npu/commands/translate.md"),
        }
    }

    #[test]
    fn describe_produces_valid_json_with_declared_args_and_output_contract() {
        let spec = sample_translate_spec();

        let json_text =
            describe(&spec, &crate::config::Config::default()).expect("describe must succeed");
        let value: serde_json::Value =
            serde_json::from_str(&json_text).expect("describe must produce valid JSON");

        assert_eq!(value["name"], "translate");
        assert_eq!(value["description"], "Translate input text");
        assert_eq!(value["model"], "qwen-fast");
        assert_eq!(value["input"], "stdin_or_file");
        assert_eq!(value["args"]["language"]["short"], "l");
        assert_eq!(value["args"]["language"]["required"], true);
        assert_eq!(value["args"]["language"]["description"], "Target language");
        assert_eq!(value["output"]["format"], "json");
        assert_eq!(value["output"]["schema"], "schemas/translation.json");
    }

    #[test]
    fn describe_text_output_without_schema_has_a_null_schema_field() {
        let mut spec = sample_translate_spec();
        spec.output = crate::output::OutputSpec {
            format: crate::output::Format::Text,
            schema: None,
            max_lines: Some(1),
            allow_truncated: false,
            strip_reasoning: false,
        };

        let json_text =
            describe(&spec, &crate::config::Config::default()).expect("describe must succeed");
        let value: serde_json::Value =
            serde_json::from_str(&json_text).expect("describe must produce valid JSON");

        assert_eq!(value["output"]["format"], "text");
        assert!(value["output"]["schema"].is_null());
        assert_eq!(value["output"]["max_lines"], 1);
    }

    // -- parse_host_port -------------------------------------------------------

    #[test]
    fn parse_host_port_defaults_http_to_port_80() {
        assert_eq!(
            parse_host_port("http://127.0.0.1").expect("must parse"),
            ("127.0.0.1".to_string(), 80)
        );
    }

    #[test]
    fn parse_host_port_defaults_https_to_port_443() {
        assert_eq!(
            parse_host_port("https://example.com").expect("must parse"),
            ("example.com".to_string(), 443)
        );
    }

    #[test]
    fn parse_host_port_explicit_port_is_used() {
        assert_eq!(
            parse_host_port("http://127.0.0.1:8000").expect("must parse"),
            ("127.0.0.1".to_string(), 8000)
        );
    }

    #[test]
    fn parse_host_port_tolerates_a_path_after_the_authority() {
        assert_eq!(
            parse_host_port("http://127.0.0.1:8000/v3/chat").expect("must parse"),
            ("127.0.0.1".to_string(), 8000)
        );
    }

    #[test]
    fn parse_host_port_missing_scheme_is_an_error() {
        assert!(parse_host_port("127.0.0.1:8000").is_err());
    }

    #[test]
    fn parse_host_port_unsupported_scheme_is_an_error() {
        assert!(parse_host_port("ftp://127.0.0.1:21").is_err());
    }

    // -- parse_host_port: bracketed IPv6 ----------------------------------

    #[test]
    fn parse_host_port_ipv6_with_explicit_port_strips_brackets_from_host() {
        // The returned host must be WITHOUT brackets: `Ipv6Addr::from_str`
        // (used by `ToSocketAddrs` in `tcp_probe`) rejects the bracketed
        // form, cf. `parse_ipv6_authority`'s doc.
        assert_eq!(
            parse_host_port("http://[::1]:8000").expect("must parse"),
            ("::1".to_string(), 8000)
        );
    }

    #[test]
    fn parse_host_port_ipv6_without_port_defaults_to_scheme_port() {
        assert_eq!(
            parse_host_port("https://[2001:db8::1]").expect("must parse"),
            ("2001:db8::1".to_string(), 443)
        );
    }

    #[test]
    fn parse_host_port_ipv6_tolerates_a_path_after_the_authority() {
        assert_eq!(
            parse_host_port("http://[::1]:8000/v3/chat").expect("must parse"),
            ("::1".to_string(), 8000)
        );
    }

    #[test]
    fn parse_host_port_ipv6_unclosed_bracket_is_an_error_not_a_panic() {
        assert!(parse_host_port("http://[::1:8000").is_err());
    }

    #[test]
    fn parse_host_port_ipv6_empty_brackets_is_an_error() {
        assert!(parse_host_port("http://[]:8000").is_err());
    }

    #[test]
    fn parse_host_port_ipv6_trailing_colon_without_port_is_an_error() {
        assert!(parse_host_port("http://[::1]:").is_err());
    }

    #[test]
    fn parse_host_port_ipv6_garbage_after_bracket_is_an_error_not_a_panic() {
        assert!(parse_host_port("http://[::1]garbage").is_err());
    }

    #[test]
    fn parse_host_port_ipv6_non_numeric_port_is_an_error() {
        assert!(parse_host_port("http://[::1]:notaport").is_err());
    }

    // -- tcp_probe --------------------------------------------------------------

    #[test]
    fn tcp_probe_succeeds_against_a_locally_bound_listener() {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind of the ephemeral listener");
        let addr = listener.local_addr().expect("listener's local address");
        let base_url = format!("http://{addr}");

        // Accepts the incoming connection then finishes: no thread nor
        // socket must survive this test.
        let acceptor = std::thread::spawn(move || {
            let _ = listener.accept();
        });

        let result = tcp_probe(&base_url);

        acceptor.join().expect("the acceptor thread must not panic");
        assert!(result.is_ok(), "got: {result:?}");
    }

    #[test]
    fn tcp_probe_fails_against_a_closed_port() {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind of the ephemeral listener");
        let addr = listener.local_addr().expect("listener's local address");
        drop(listener); // closes immediately: nobody is listening here anymore.

        let result = tcp_probe(&format!("http://{addr}"));

        assert!(
            result.is_err(),
            "a closed port must be reported as unreachable"
        );
    }

    // -- serve -------------------------------------------------------------

    /// Stub runner: records the argument list it was given, and returns a
    /// container identifier. `serve` never spawns a process itself, so no
    /// test of this module needs Docker installed.
    fn capturing_runner(
        captured: &std::cell::RefCell<Vec<String>>,
    ) -> impl Fn(&[String]) -> crate::Result<String> + '_ {
        move |args: &[String]| {
            // `serve` asks "does this container already exist?" before
            // starting anything; answering with a container name would make
            // every fixture look already-served. Empty means absent, and the
            // probe is not what these tests capture.
            if args.first().is_some_and(|verb| verb == "ps") {
                return Ok(String::new());
            }
            captured.replace(args.to_vec());
            Ok("3f2a9c1b8e40".to_string())
        }
    }

    fn test_env(name: &str) -> Option<String> {
        match name {
            "NPU_TEST_MODELS" => Some("/home/tester/models".to_string()),
            _ => None,
        }
    }

    fn containerized_config() -> crate::config::Config {
        let mut config = crate::config::Config::default();
        config
            .backends
            .insert("ovms".to_string(), containerized_backend("ovms"));
        config
            .models
            .insert("qwen".to_string(), model("qwen", "ovms", "chat"));
        config
    }

    #[test]
    fn serve_builds_docker_run_with_options_then_image_then_args() {
        let config = containerized_config();
        let captured = std::cell::RefCell::new(Vec::new());

        let id = serve(
            &config,
            "qwen",
            &test_env,
            &capturing_runner(&captured),
            &test_host(),
        )
        .expect("serving a containerized backend must succeed");

        assert_eq!(id, "3f2a9c1b8e40");
        assert_eq!(
            captured.into_inner(),
            vec![
                "run",
                "-d",
                "--name",
                "npu-ovms",
                "-p",
                "8000:8000",
                "-v",
                "/home/tester/models:/models",
                "example/server:latest",
                "--source_model",
                "qwen-underlying",
            ]
        );
    }

    #[test]
    fn serve_unknown_model_is_a_config_error_naming_the_model() {
        let config = containerized_config();
        let captured = std::cell::RefCell::new(Vec::new());

        let err = serve(
            &config,
            "absent",
            &test_env,
            &capturing_runner(&captured),
            &test_host(),
        )
        .expect_err("an unknown model must fail");

        assert!(matches!(err, crate::Error::Config(_)));
        assert!(
            err.to_string().contains("absent"),
            "the message must name the faulty model"
        );
        assert!(
            captured.into_inner().is_empty(),
            "no container must be started for an unknown model"
        );
    }

    #[test]
    fn serve_backend_without_docker_table_is_a_config_error_naming_the_backend() {
        let mut config = crate::config::Config::default();
        config.backends.insert(
            "plain".to_string(),
            backend("plain", "http://127.0.0.1:8000", &["chat"]),
        );
        config
            .models
            .insert("qwen".to_string(), model("qwen", "plain", "chat"));
        let captured = std::cell::RefCell::new(Vec::new());

        let err = serve(
            &config,
            "qwen",
            &test_env,
            &capturing_runner(&captured),
            &test_host(),
        )
        .expect_err("a backend without [docker] cannot be served");

        assert!(matches!(err, crate::Error::Config(_)));
        assert!(
            err.to_string().contains("plain"),
            "the message must name the backend that cannot be started"
        );
        assert!(captured.into_inner().is_empty());
    }

    #[test]
    fn serve_propagates_the_runner_failure() {
        let config = containerized_config();
        let failing = |_args: &[String]| Err(crate::Error::backend("docker absent"));

        let err = serve(&config, "qwen", &test_env, &failing, &test_host())
            .expect_err("a failing runner must fail the command");

        assert!(matches!(err, crate::Error::Backend(_)));
        assert_eq!(err.exit_code(), 3);
    }

    #[test]
    fn serve_undefined_environment_variable_is_a_config_error_naming_it() {
        let config = containerized_config();
        let empty_env = |_name: &str| None;
        let captured = std::cell::RefCell::new(Vec::new());

        let err = serve(
            &config,
            "qwen",
            &empty_env,
            &capturing_runner(&captured),
            &test_host(),
        )
        .expect_err("an undefined variable referenced by [docker] must fail");

        assert!(matches!(err, crate::Error::Config(_)));
        assert!(
            err.to_string().contains("NPU_TEST_MODELS"),
            "the message must name the undefined variable"
        );
        assert!(captured.into_inner().is_empty());
    }

    // -- doctor: check (f), container runtime ------------------------------

    #[test]
    fn doctor_produces_no_container_check_when_no_backend_declares_docker() {
        let mut config = crate::config::Config::default();
        config.backends.insert(
            "plain".to_string(),
            backend("plain", "http://127.0.0.1:8000", &["chat"]),
        );

        let checks = doctor(
            Some(&config),
            Some(&[]),
            None,
            &probes(&always_ok, &container_fails, &command_ok),
        );

        assert!(
            checks
                .iter()
                .all(|check| matches!(check.status, Status::Ok)),
            "a configuration without [docker] must not be penalized by a missing runtime"
        );
        assert_eq!(doctor_exit_code(&checks), 0);
    }

    #[test]
    fn doctor_container_runtime_failure_is_reachability_and_yields_exit_code_three() {
        let config = containerized_config();

        let checks = doctor(
            Some(&config),
            Some(&[]),
            None,
            &probes(&always_ok, &container_fails, &command_ok),
        );

        let failed: Vec<&Check> = checks
            .iter()
            .filter(|check| matches!(check.status, Status::Failed(_)))
            .collect();
        assert_eq!(failed.len(), 1, "only the container check must fail");
        assert_eq!(failed[0].kind, CheckKind::Reachability);
        assert_eq!(doctor_exit_code(&checks), 3);
    }

    #[test]
    fn doctor_container_runtime_available_passes_when_a_backend_declares_docker() {
        let config = containerized_config();

        let checks = doctor(
            Some(&config),
            Some(&[]),
            None,
            &probes(&always_ok, &container_ok, &command_ok),
        );

        assert!(
            checks
                .iter()
                .any(|check| check.kind == CheckKind::Reachability
                    && check.label.contains("container")),
            "a declared [docker] table must produce its own check"
        );
        assert_eq!(doctor_exit_code(&checks), 0);
    }

    // -- lifecycle: stop / status / logs -----------------------------------

    #[test]
    fn stop_removes_the_container_named_after_the_backend() {
        let config = containerized_config();
        let captured = std::cell::RefCell::new(Vec::new());

        stop(&config, "qwen", &capturing_runner(&captured), &test_host())
            .expect("stopping must succeed");

        assert_eq!(
            captured.into_inner(),
            vec!["rm", "--force", "npu-ovms"],
            "stop must REMOVE the container: a stopped one still owns its name"
        );
    }

    #[test]
    fn stop_returns_the_container_name_when_the_runtime_printed_nothing() {
        let config = containerized_config();
        // `docker rm --force` on an absent container succeeds without
        // printing: stdout must still carry a result, never a blank line.
        let silent = |_args: &[String]| Ok(String::new());

        let result =
            stop(&config, "qwen", &silent, &test_host()).expect("stopping must stay idempotent");

        assert_eq!(result, "npu-ovms");
    }

    #[test]
    fn stop_backend_without_docker_table_is_a_config_error_naming_the_backend() {
        let mut config = crate::config::Config::default();
        config.backends.insert(
            "plain".to_string(),
            backend("plain", "http://127.0.0.1:8000", &["chat"]),
        );
        config
            .models
            .insert("qwen".to_string(), model("qwen", "plain", "chat"));
        let captured = std::cell::RefCell::new(Vec::new());

        let err = stop(&config, "qwen", &capturing_runner(&captured), &test_host())
            .expect_err("npu only manages the containers it starts");

        assert!(matches!(err, crate::Error::Config(_)));
        assert!(err.to_string().contains("plain"));
        assert!(captured.into_inner().is_empty());
    }

    #[test]
    fn logs_follow_adds_the_flag_and_streams_without_capturing() {
        let config = containerized_config();
        let captured = std::cell::RefCell::new(Vec::new());
        let streamer = |args: &[String]| {
            captured.replace(args.to_vec());
            Ok(())
        };

        logs(&config, "qwen", true, &streamer, &test_host(), &unused_sink)
            .expect("streaming must succeed");

        assert_eq!(captured.into_inner(), vec!["logs", "--follow", "npu-ovms"]);
    }

    #[test]
    fn logs_without_follow_omits_the_flag() {
        let config = containerized_config();
        let captured = std::cell::RefCell::new(Vec::new());
        let streamer = |args: &[String]| {
            captured.replace(args.to_vec());
            Ok(())
        };

        logs(
            &config,
            "qwen",
            false,
            &streamer,
            &test_host(),
            &unused_sink,
        )
        .expect("streaming must succeed");

        assert_eq!(captured.into_inner(), vec!["logs", "npu-ovms"]);
    }

    #[test]
    fn status_reports_a_containerized_backend_with_the_runtime_state() {
        let config = containerized_config();
        let runner = |_args: &[String]| Ok("Up 3 minutes".to_string());

        let report = status(&config, &runner, &test_host()).expect("status never fails");

        assert!(report.contains("ovms"), "got: {report}");
        assert!(report.contains("npu-ovms"), "got: {report}");
        assert!(report.contains("Up 3 minutes"), "got: {report}");
    }

    /// One table reports every family, so each line must say which family
    /// it belongs to — otherwise a PID and a container name share a column
    /// with nothing to tell them apart.
    #[test]
    fn status_names_the_runtime_family_of_each_line() {
        let config = containerized_config();
        let runner = |_args: &[String]| Ok("Up 3 minutes".to_string());

        let report = status(&config, &runner, &test_host()).expect("status never fails");

        assert!(
            report.contains(crate::runtime::docker::NAME),
            "got: {report}"
        );
    }

    #[test]
    fn status_reports_a_missing_container_as_not_started_rather_than_failing() {
        let config = containerized_config();
        let runner = |_args: &[String]| Ok(String::new());

        let report = status(&config, &runner, &test_host())
            .expect("an absent container is a state, not an error");

        assert!(report.contains(NOT_STARTED), "got: {report}");
    }

    #[test]
    fn status_survives_a_runtime_that_cannot_be_asked() {
        let config = containerized_config();
        let runner = |_args: &[String]| Err(crate::Error::backend("docker absent"));

        let report = status(&config, &runner, &test_host())
            .expect("a report must not die on its first bad line");

        assert!(report.contains("ovms"), "got: {report}");
    }

    #[test]
    fn status_ignores_backends_without_a_docker_table_but_keeps_its_header() {
        let mut config = crate::config::Config::default();
        config.backends.insert(
            "plain".to_string(),
            backend("plain", "http://127.0.0.1:8000", &["chat"]),
        );
        let runner = |_args: &[String]| Ok("Up".to_string());

        let report = status(&config, &runner, &test_host()).expect("status never fails");

        assert!(!report.contains("plain"), "got: {report}");
        assert!(
            report.contains("BACKEND"),
            "the header must survive an empty table"
        );
    }

    // -- the process runtime family -----------------------------------------

    /// Unique fixture directory: tests run in parallel and a fixed path
    /// collides (same idiom as every other module's).
    fn fixture_state_env(name: &str) -> crate::runtime::state::StateEnv {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-fixtures")
            .join(format!("builtin-{name}-{n}"));
        std::fs::create_dir_all(&dir).expect("fixture directory creation");
        crate::runtime::state::StateEnv {
            xdg_state_home: Some(dir.clone()),
            home: Some(dir),
        }
    }

    /// A backend served by a process runtime, whose command deliberately
    /// does not exist: every test below asserts on a REFUSAL, so nothing in
    /// this module ever spawns anything.
    fn process_backend(id: &str, command: &str) -> crate::config::Backend {
        let mut backend = backend(id, "http://127.0.0.1:8000", &["chat"]);
        backend.runtime = Some(crate::config::Runtime::Process(crate::config::Process {
            command: command.to_string(),
            arguments: vec!["{{ args.model }}".to_string()],
            env: std::collections::BTreeMap::new(),
            startup_timeout_secs: 30,
        }));
        backend
    }

    fn process_config(command: &str) -> crate::config::Config {
        let mut config = crate::config::Config::default();
        config
            .backends
            .insert("local".to_string(), process_backend("local", command));
        config
            .models
            .insert("qwen".to_string(), model("qwen", "local", "chat"));
        config
    }

    /// A machine that never asked for a process runtime must not be
    /// penalized by a check about one — the same rule as the container
    /// runtime's.
    #[test]
    fn doctor_produces_no_runtime_command_check_without_a_process_backend() {
        let mut config = crate::config::Config::default();
        config.backends.insert(
            "ovms".to_string(),
            backend("ovms", "http://127.0.0.1:8000", &["chat"]),
        );

        let checks = doctor(
            Some(&config),
            Some(&[]),
            None,
            &probes(&always_ok, &container_ok, &command_fails),
        );

        assert!(
            checks.iter().all(|c| !c.label.contains("runtime command")),
            "got: {checks:?}"
        );
        assert_eq!(doctor_exit_code(&checks), 0);
    }

    /// A declared command produces its own check, NAMING it: a report that
    /// only said "a command is missing" would send its reader through every
    /// backend file.
    #[test]
    fn doctor_checks_each_declared_runtime_command_by_name() {
        let config = process_config("llama-server");

        let checks = doctor(
            Some(&config),
            Some(&[]),
            None,
            &probes(&always_ok, &container_ok, &command_ok),
        );

        let check = checks
            .iter()
            .find(|c| c.label.contains("llama-server"))
            .expect("the declared command must have its own check");
        assert_eq!(check.kind, CheckKind::Reachability);
        assert!(matches!(check.status, Status::Ok));
    }

    /// A `command` carrying a placeholder is only known once `npu serve`
    /// renders it against a model, and `doctor` has no model. Emitting the
    /// check anyway turned a valid configuration red and told its operator
    /// to install a binary named `{{ env.LLAMA_BIN }}`.
    #[test]
    fn doctor_emits_no_check_for_a_command_it_cannot_resolve_yet() {
        let config = process_config("{{ env.LLAMA_BIN }}");

        let checks = doctor(
            Some(&config),
            Some(&[]),
            None,
            &probes(&always_ok, &container_ok, &command_fails),
        );

        assert!(
            checks.iter().all(|c| !c.label.contains("runtime command")),
            "got: {checks:?}"
        );
        assert_eq!(doctor_exit_code(&checks), 0);
    }

    /// An absent command is something to INSTALL, not a file to fix: exit
    /// `3`, never `2`. Classified by `CheckKind`, never by the label.
    #[test]
    fn a_missing_runtime_command_is_a_reachability_failure_worth_exit_three() {
        let config = process_config("llama-server");

        let checks = doctor(
            Some(&config),
            Some(&[]),
            None,
            &probes(&always_ok, &container_ok, &command_fails),
        );

        let check = checks
            .iter()
            .find(|c| c.label.contains("llama-server"))
            .expect("the declared command must have its own check");
        assert_eq!(check.kind, CheckKind::Reachability);
        assert!(matches!(check.status, Status::Failed(_)));
        assert_eq!(doctor_exit_code(&checks), 3);
    }

    /// One command shared by several backends is probed once, and the
    /// report stays deterministic.
    #[test]
    fn a_command_shared_by_two_backends_is_probed_once() {
        let mut config = process_config("llama-server");
        config.backends.insert(
            "other".to_string(),
            process_backend("other", "llama-server"),
        );

        let probed = std::sync::atomic::AtomicU64::new(0);
        let probe = |_command: &str| {
            probed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        };

        let checks = doctor(
            Some(&config),
            Some(&[]),
            None,
            &probes(&always_ok, &container_ok, &probe),
        );

        assert_eq!(probed.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(
            checks
                .iter()
                .filter(|c| c.label.contains("llama-server"))
                .count(),
            1
        );
    }

    /// Dispatch: a process backend must reach `runtime::process`, which is
    /// what the message about the absent COMMAND (never an image, never a
    /// container) proves.
    #[test]
    fn serve_dispatches_a_process_backend_to_the_process_runtime() {
        let config = process_config("npu-does-not-exist-anywhere");
        let host = crate::runtime::process::Host {
            env: &|_| None,
            state: fixture_state_env("serve-dispatch"),
            inspect: &unused_inspect,
            signal: &unused_signal,
            probe: &unused_probe,
        };

        let err = serve(&config, "qwen", &unused_env, &unused_runner, &host)
            .expect_err("the command does not exist");

        assert_eq!(err.exit_code(), 3);
        let message = err.to_string();
        assert!(message.contains("npu-does-not-exist-anywhere"), "{message}");
        assert!(message.contains("local"), "{message}");
    }

    /// `stop` is idempotent for this family too: nothing started is a
    /// success, and the result on stdout is the backend identifier.
    #[test]
    fn stop_on_a_never_served_process_backend_succeeds() {
        let config = process_config("llama-server");
        let host = crate::runtime::process::Host {
            env: &|_| None,
            state: fixture_state_env("stop-dispatch"),
            inspect: &unused_inspect,
            signal: &unused_signal,
            probe: &unused_probe,
        };

        assert_eq!(
            stop(&config, "qwen", &unused_runner, &host).expect("stopping nothing is a success"),
            "local"
        );
    }

    /// Both families in one table, each line saying which one it belongs
    /// to — the reason the RUNTIME column exists at all.
    #[test]
    fn status_reports_a_process_backend_beside_a_containerized_one() {
        let mut config = crate::config::Config::default();
        config
            .backends
            .insert("ovms".to_string(), containerized_backend("ovms"));
        config.backends.insert(
            "local".to_string(),
            process_backend("local", "llama-server"),
        );
        let host = crate::runtime::process::Host {
            env: &|_| None,
            state: fixture_state_env("status-dispatch"),
            inspect: &unused_inspect,
            signal: &unused_signal,
            probe: &unused_probe,
        };

        let report =
            status(&config, &|_args| Ok(String::new()), &host).expect("status never fails");

        let lines: Vec<&str> = report.lines().collect();
        assert_eq!(lines.len(), 3, "{report}");
        let local = lines
            .iter()
            .find(|line| line.starts_with("local"))
            .expect("the process backend must have a line");
        assert!(local.contains(crate::runtime::process::NAME), "{local}");
        assert!(local.contains(NOT_STARTED), "{local}");
    }

    /// A report that dies on its first unreadable line is not a report: a
    /// corrupt record must cost its own row and nothing else.
    #[test]
    fn a_corrupt_process_record_does_not_suppress_the_other_rows() {
        let mut config = crate::config::Config::default();
        config
            .backends
            .insert("ovms".to_string(), containerized_backend("ovms"));
        let local = process_backend("local", "llama-server");
        let state = fixture_state_env("status-corrupt");
        let dir = crate::runtime::state::state_dir(&state).expect("a home is set");
        std::fs::create_dir_all(&dir).expect("the state directory");
        // Planted where `status` will look: a record's name is keyed on the
        // backend FILE as well as on the identifier, so a path built from
        // the identifier alone would be read by nobody and this test would
        // pass without exercising anything.
        let path = crate::runtime::state::state_path(&state, &local.id, &local.source)
            .expect("a valid identifier");
        std::fs::write(&path, "{ not json").expect("the planted record");
        config.backends.insert("local".to_string(), local);

        let host = crate::runtime::process::Host {
            env: &|_| None,
            state,
            inspect: &unused_inspect,
            signal: &unused_signal,
            probe: &unused_probe,
        };

        let report = status(&config, &|_args| Ok(String::new()), &host)
            .expect("a report must not die on its first bad line");

        assert_eq!(report.lines().count(), 3, "{report}");
        assert!(report.contains("npu-ovms"), "{report}");
    }

    /// Dispatch again: a process backend's logs come from the file `serve`
    /// wrote, so the streamer must stay untouched and the message must name
    /// the backend.
    #[test]
    fn logs_of_a_never_served_process_backend_name_the_backend() {
        let config = process_config("llama-server");
        let host = crate::runtime::process::Host {
            env: &|_| None,
            state: fixture_state_env("logs-dispatch"),
            inspect: &unused_inspect,
            signal: &unused_signal,
            probe: &unused_probe,
        };

        let err = logs(
            &config,
            "qwen",
            false,
            &|_args| panic!("a process backend must not go through the container streamer"),
            &host,
            &|_bytes| Ok(()),
        )
        .expect_err("there is nothing to read");

        assert_eq!(err.exit_code(), 3);
        assert!(err.to_string().contains("local"), "{err}");
    }
}

# npu — working notes for Claude

Generic CLI execution engine in Rust. Commands, models, backends and output
schemas are **configuration**, never code. The core understands execution
mechanics, not AI business semantics — adding a command must never require a
rebuild.

## The one gate

```sh
make qa      # fmt --check + clippy (all + pedantic, -D warnings) + tests + cargo-deny
make fix     # rustfmt and clippy autofixes
```

`make qa` must pass before any commit. Nothing else is a substitute.

## Architecture rule

> Every configuration key that is read must be honoured, or rejected with an
> actionable message naming the offending file. A key read and then silently
> ignored is a defect, not a shortcut.

This is why every `Deserialize` struct for a configuration file carries
`#[serde(deny_unknown_fields)]`, why `type` and `method` are validated instead
of assumed, and why `[timeouts]` — specified but unimplemented — is rejected
rather than accepted and dropped. Adding a field you read but do not use
breaks this rule.

## Exit-code contract — do not change

| Code | Variant | Meaning |
| ---: | --- | --- |
| `1` | `Error::Io` | unreadable file, broken pipe |
| `2` | `Error::Config` | the user's files are wrong |
| `3` | `Error::Backend` | unreachable, or non-2xx |
| `4` | `Error::Output` | the model's answer violated the declared contract |

`npu` is designed to be driven by other programs; these codes are the API.
Two consequences already baked in: `panic = "unwind"` in release (abort would
turn an exit code into SIGABRT), and `doctor` classifies a check by
`CheckKind`, **never** by the text of its label.

**stdout carries the command result and nothing else** — on every path,
including failures, where it must be zero bytes.

## Tests

- **Never assert on the wording of a message.** Prose can be reformulated or
  translated without functional impact; freezing it turns every rewording into
  a build failure.
- Do assert on: exit codes, `Error` variants, stdout purity, return values of
  pure functions, and the fact that a message **names the offending
  identifier** (file path, backend/model id, argument name). A message that
  stops naming the faulty file is a functional regression for a calling agent.
- Fixtures go under `target/test-fixtures/<name>-<counter>` with an atomic
  counter — tests run in parallel and fixed paths collide.

## Edition 2024 and `unsafe_code = "forbid"`

`std::env::set_var` is `unsafe` in edition 2024, so it is forbidden here. Any
environment-dependent logic therefore **takes its environment as a parameter**
(`ScopeEnv`, `env: &dyn Fn(&str) -> Option<String>`, `probe: &dyn Fn(...)`),
which is what makes it testable. Integration tests set variables on the
**child process** via `Command::env`/`env_remove`, never on the test process.

`unwrap_used`, `expect_used` and `panic` are warnings — and warnings are
errors under `-D warnings`. Tests opt out with
`#![allow(clippy::expect_used)]` at the top of the file.

## Manifest gotcha

Overriding an individual lint of a group requires `priority = -1` on the group
itself, or Cargo rejects the manifest:

```toml
[lints.clippy]
all = { level = "warn", priority = -1 }
unwrap_used = "warn"
```

## Dependencies

Seven, deliberately: `clap` (builder API, not derive — the command tree is
built at runtime from a directory scan), `serde`, `serde_json`, `toml`,
`ureq` (blocking, rustls — chosen over `reqwest`, which drags in tokio),
`jsonschema` with `default-features = false` (its defaults pull `reqwest`
back in via `resolve-http`).

Adding one is a measured decision: check the binary size and the crate count
before and after, and record the numbers.

## Documentation

`README.md` and `docs/` are English and quote the binary **verbatim**. Before
changing a quoted block, run the binary and paste what it actually printed —
never translate or reconstruct it by hand.

`IMPLEMENTATION.md` and `npu-cli-spec.md` are local working documents, not in
the repository and git-ignored. Do not reference them from code or docs.

## Conventions

- Source, comments, rustdoc, messages and documentation: **English**.
  Commit messages: **French**, Conventional Commits.
- Non-ASCII test fixtures (`café`, `Montréal`) are deliberate: they exercise
  multibyte boundaries and non-ASCII argument rejection. Leave them.

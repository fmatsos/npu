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

CI runs that same target and nothing else (`.github/workflows/qa.yml`):
reproducing its steps in YAML would let CI and a developer's machine drift
apart, and this repository has one definition of green.

## Releasing

A release is a tag. `.github/workflows/release.yml` fires on `vX.Y.Z`,
re-runs the gate, builds the eight release archives and publishes the GitHub
release with the notes **extracted verbatim from `CHANGELOG.md`** (falling
back to generated notes when the section is missing). Nothing is released by
hand: a local `gh release create` would skip the gate and publish binaries
built on someone's machine.

The `release` skill (`skills/release/SKILL.md`) owns the part that needs
judgement — version from the Conventional Commits since the last tag
(pre-1.0: a breaking change bumps the MINOR), the `CHANGELOG.md` section in
Keep a Changelog form, one bullet per change linking to its commit by full
sha, and the closing compare link. Anything touching the exit-code contract,
the stdout contract, a reserved name or the meaning of a configuration key is
breaking whatever its commit type claims.

## Architecture rule

> Every configuration key that is read must be honoured, or rejected with an
> actionable message naming the offending file. A key read and then silently
> ignored is a defect, not a shortcut.

This is why every `Deserialize` struct for a configuration file carries
`#[serde(deny_unknown_fields)]`, why `type` and `method` are validated instead
of assumed, and why a backend's optional `[timeouts]` table only accepts the
one key `backend.rs` actually reads (`request_secs`) — any other key inside
it is rejected, not silently ignored. Adding a field you read but do not use
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
including failures, where it must be zero bytes. `--verbose error|warn|info`
moves a threshold on the DIAGNOSTIC stream (`log.rs`, stderr only); no level
may ever add or remove a byte on stdout.

## Built-ins and the container lifecycle

Seven built-ins, all listed in `builtin::RESERVED` (a command file whose first
path segment matches one is rejected at load time): `doctor`, `models`,
`describe`, and the lifecycle — `serve`, `stop`, `status`, `logs`.

The lifecycle drives Docker, which is an **optional** prerequisite. The core
knows the shape of a `docker run` invocation and nothing else: image, options
and arguments come from the backend's optional `[docker]` table, so changing
image, ports or accelerator is a configuration change, never a rebuild. The
container is named `npu-<backend-id>`, which is how `stop`/`status`/`logs`
find it again.

Everything that touches the outside world is **injected**, like `probe` in
`doctor`: `runner` (captures stdout), `streamer` (inherits both streams, for
`logs`), `container_probe`. No test in the suite needs Docker installed, and
`std::process::Command` appears in exactly one module (`builtin.rs`).

`doctor`'s container check only exists when a backend declares `[docker]` — a
machine that never asked for a container must not be penalized — and it is a
`Reachability` check, so a missing runtime gives `3`, never `2`.

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

## Command files

Frontmatter is fenced by `---`, TOML inside. A file still opening with `+++`
(the delimiter of earlier versions) is rejected with its own message naming
both delimiters — never diagnosed as "missing frontmatter", which would send
the author looking for a line that is right there.

Reserved argument names: `help`, `version`, `FILE`, `verbose`. Reserved short
letters: `-h`, `-v`. `verbose`/`-v` are reserved because `lib.rs` declares a
GLOBAL `--verbose` on the root command, and a collision makes `clap` panic at
build time — which is not an acceptable way to report a configuration error.

## Conventions

- Source, comments, rustdoc, messages, documentation and **commit messages**:
  **English**, Conventional Commits. The whole history was translated in one
  pass; do not reintroduce French in a message.
- Non-ASCII test fixtures (`café`, `Montréal`) are deliberate: they exercise
  multibyte boundaries and non-ASCII argument rejection. Leave them.

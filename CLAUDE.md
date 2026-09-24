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

The `release` skill (`.claude/skills/release/SKILL.md`) owns the part that needs
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

The built-ins live under six names, all listed in `builtin::RESERVED` with
`help` (a command file whose first path segment matches one is rejected at
load time): the `backend` group — the lifecycle, `serve`, `stop`, `status`,
`logs`, `tune` —, the `config` group — `check`, `models` —, the `model`
group — `discover`, which needs no configuration —, `doctor` (the same
command as `config check`, kept at the top level), `describe` and `update`;
the version is the root `--version` flag. Every other name belongs to the
user's commands: do not add a top-level built-in, grow a group instead.

`npu --help` shows the configured commands and the built-ins in two
sections through a `help_template` (`lib.rs::sectioned_help`): `clap` has
no per-subcommand heading, so the built-ins are HIDDEN from its list and
rendered by hand — they parse as before. `clap`'s generated `help`
subcommand is disabled and replaced by a `help` built-in of ours
(`lib.rs::help`, which re-parses `<path> --help`), listed with the others.

Colours live in `style.rs`, and every styled byte goes out through
`anstream`, which strips them when the stream is not a terminal and honours
`NO_COLOR`: a pipe receives exactly the bytes it did before colours existed.
Never `println!` a styled string — `anstream::println!` or nothing.

`describe` resolves built-ins first, from the `clap` tree `add_builtins`
builds — the single declaration of a built-in, so its description cannot
drift — then configured commands.

A backend declares how it is started by the optional TAGGED `[runtime]`
table (`type = "docker"`), deserialized into `config::Runtime`. The untagged
`[docker]` table of earlier versions is still accepted and FOLDED into
`runtime` at load time, once, so every reader downstream sees one shape;
declaring both is rejected, naming the file and the backend. `.docker` is
therefore read in exactly two places, both in `config.rs`: the fold and the
rule that rejects the double declaration. Both fields are `pub(crate)` so
that rule is structural, not a doc comment: everything else goes through
`Backend::runtime()`, or through `config::docker_of` — the crate's single
runtime-family test, an exhaustive `match` a new variant breaks.

The lifecycle drives Docker, which is an **optional** prerequisite. The core
knows the shape of a `docker run` invocation and nothing else: image, options
and arguments come from the backend's `[runtime]` table, so changing image,
ports or accelerator is a configuration change, never a rebuild. The
container is named `npu-<backend-id>`, which is how `backend stop`/`status`/`logs`
find it again.

`builtin.rs` is ORCHESTRATION: it resolves model and backend, `match`es on
the runtime family, and formats. `src/runtime/` holds what actually touches
the outside world — dispatch is a `match`, never a trait object, so a new
family is a compile error at every site that must handle it.

Everything that touches the outside world is **injected**, like `probe` in
`doctor`: `runner` (captures stdout), `streamer` (inherits both streams, for
`logs`), `container_probe`. No test in the suite needs Docker installed, and
`std::process::Command` appears only in `src/runtime/`.

`doctor`'s container check only exists when a backend declares a Docker
runtime — a machine that never asked for a container must not be penalized —
and it is a `Reachability` check, so a missing runtime gives `3`, never `2`.

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

Twelve, deliberately: `clap` (builder API, not derive — the command tree is
built at runtime from a directory scan), `serde`, `serde_json`, `toml`,
`ureq` (blocking, rustls — chosen over `reqwest`, which drags in tokio),
`jsonschema` with `default-features = false` (its defaults pull `reqwest`
back in via `resolve-http`), the three `npu update` brought in:
`semver` (comparing the release manifest's version to the running one),
`sha2` (verifying the downloaded binary before it replaces anything) and
`self-replace` (replacing the running executable at its own path), and
`sysinfo` with `default-features = false, features = ["system"]` — the
process-runtime family needs process identity (`start_time`, `exe`) and
signalling (`kill_with`) through a wholly SAFE API, which
`unsafe_code = "forbid"` makes non-negotiable; the four unused default
features would also drag `rayon` in via `multithread`, and `indicatif`
with `default-features = false` — spinners and progress bars, wrapped by
`progress.rs`, which is the only module allowed to draw: stderr only, only
when stderr is a terminal and the level is above `error`, cleared on drop.
It cost 136 400 bytes on the release binary (8 540 480 -> 8 676 880) and
four crates (108 -> 112); and `anstream`, the stream `clap`'s `color`
feature already pulls in, used directly so that our own colours are
stripped by the same rule as `clap`'s. The feature and `anstream` together
cost 90 312 bytes (8 676 880 -> 8 767 192) and six crates (112 -> 118).

Adding one is a measured decision: check the binary size and the crate count
before and after, and record the numbers. A version bump of an existing
dependency (Dependabot's weekly PRs) needs no measurement.

At 0.5.1: 118 crates in `cargo tree --edges normal` (the metric of the deltas above; `Cargo.lock` lists 205 packages, other targets included), 9 015 408 bytes release binary on
x86_64 Linux.

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
- A `ponytail:` comment marks a deliberate trade-off: it names its ceiling
  and the upgrade path. Comments state the rule, never where it came from
  (no phase, review or section numbers — `make lint` rejects them).
- Non-ASCII test fixtures (`café`, `Montréal`) are deliberate: they exercise
  multibyte boundaries and non-ASCII argument rejection. Leave them.

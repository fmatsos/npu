# Changelog

Every notable change to `npu`, newest first. Versions follow
[semantic versioning](https://semver.org); pre-1.0, a breaking change bumps
the minor.

## [0.4.0] - 2026-09-23

### Added

- `npu describe` describes built-ins as well as configured commands, and takes the path as words
  (`npu describe backend serve`, `npu describe git review`; `git/review` still works). Every
  description carries `kind` (`builtin` or `command`); a built-in lists its arguments,
  subcommands and whether it runs with a broken configuration — which `describe` itself does for
  built-ins; a configured command adds its resolved `backend`, its `fallback` and its `source`
  (the file that won and its scope). No existing field changes
  ([`770867a`](https://github.com/fmatsos/npu/commit/770867add1cc492e8c350d2ad0e9f803073ce9c5))
- On a terminal, a spinner while a model is waited on (relabelled when the fallback takes over)
  and while a process backend starts, and a progress bar while `npu update` downloads. Drawn on
  stderr only, only when stderr is a terminal and `--verbose` is above `error`
  ([`fd18b97`](https://github.com/fmatsos/npu/commit/fd18b97924d086fd4d5b0948192cced9dd836d22))
- Colours on a terminal: help and usage errors, the `warn`/`error`/`info` labels, the final error
  line and `doctor`'s check marks. Through a pipe, or with `NO_COLOR`, output is byte for byte what
  it was without them
  ([`c2c04e9`](https://github.com/fmatsos/npu/commit/c2c04e91104e57cbd1897a07665b8c7ce2aec39d))

### Changed

- **Breaking**: the built-ins are grouped. Update scripts as follows:

  | Before | Now |
  | --- | --- |
  | `npu serve`, `stop`, `status`, `logs` | `npu backend serve`, `stop`, `status`, `logs` |
  | `npu models` | `npu config models` |
  | `npu version` | `npu --version`, which prints `npu X.Y.Z` |

  `npu doctor` is unchanged and also available as `npu config check`. The old names are no longer
  recognised (exit `2`, nothing on stdout). The reserved command names shrink to `backend`,
  `config`, `doctor`, `describe`, `update` and `help`: a command file may now be named `serve`,
  `stop`, `status`, `logs`, `models` or `version`
  ([`83130e3`](https://github.com/fmatsos/npu/commit/83130e36f3deb907ec865378cdf218ecb4f713aa))
- `npu --help` lists the configured commands and the built-ins in two separate sections,
  `Commands:` and `Built-ins:`; `npu help <command>` is listed among the built-ins
  ([`c556e6e`](https://github.com/fmatsos/npu/commit/c556e6e2b066030d2b3451890aafa71dcdb46684)) ([`c2c04e9`](https://github.com/fmatsos/npu/commit/c2c04e91104e57cbd1897a07665b8c7ce2aec39d))

**Full changelog**: [`v0.3.1...v0.4.0`](https://github.com/fmatsos/npu/compare/v0.3.1...v0.4.0)

## [0.3.1] - 2026-09-23

### Fixed

- `npu update` and `npu version` no longer warn about an invalid configuration: neither reads it.
  After an update, the newly installed binary checks the configuration instead of the old one, so
  a key introduced by the new release no longer looks invalid; if the new version does reject it,
  a warning on stderr points to this changelog and the documentation, and the update still exits
  `0`
  ([`3e2fb18`](https://github.com/fmatsos/npu/commit/3e2fb187293dfac95e7d0414ab7b3b4ec3e80966))

**Full changelog**: [`v0.3.0...v0.3.1`](https://github.com/fmatsos/npu/compare/v0.3.0...v0.3.1)

## [0.3.0] - 2026-09-22

### Added

- A backend can be started as a **local process** instead of a container: `[runtime]` with
  `type = "process"`, a `command`, its `arguments`, an optional `[runtime.env]` overlay and a
  `startup_timeout_secs` readiness budget. `npu serve` spawns it, prints its pid only once its port
  answers, and `stop`, `status` and `logs` find it again through a small state record. This is
  what lets a Mac run `llama-server` on Metal, which a Linux container cannot reach. Unix only:
  on Windows such a backend is rejected at load time, naming the file
  ([`552881f`](https://github.com/fmatsos/npu/commit/552881fbd0d0c7483cd986c5e4db46b95d37bbd4),
  [`92d3cc5`](https://github.com/fmatsos/npu/commit/92d3cc50f1a2a06933ec7735802eef34444e1595))
- `npu stop` never signals a process it cannot prove it started: the record keeps the pid and the
  moment the system says it was born, and a recycled pid is forgotten, not killed. Two projects
  that both declare a backend `llamacpp` get separate records and logs, and neither can stop the
  other's server
  ([`552881f`](https://github.com/fmatsos/npu/commit/552881fbd0d0c7483cd986c5e4db46b95d37bbd4),
  [`92d3cc5`](https://github.com/fmatsos/npu/commit/92d3cc50f1a2a06933ec7735802eef34444e1595))
- `fallback` on a model retries once on another model when the first one fails with a backend
  error, which moves a prompt too long for an NPU-compiled graph onto a GPU-served twin. The
  primary failure is always logged at `warn`, so a backend down all day does not pass for a
  working fallback. A fallback naming an unknown model, or itself, is rejected at load time
  ([`bdd564c`](https://github.com/fmatsos/npu/commit/bdd564c9a0dd1e29990daa0ad1089060505c5468))
- `port` on a backend is declared once and read as `{{ backend.port }}` in `base_url` and the
  `[runtime]` lists, so the two can no longer drift apart. `port = "auto"` lets Docker pick a free
  port and reads it back. `npu serve` now reports a backend that is already served, or a fixed
  port held by something else, before starting anything (exit `3`)
  ([`bdd564c`](https://github.com/fmatsos/npu/commit/bdd564c9a0dd1e29990daa0ad1089060505c5468))
- Two Claude Code skills for the model side of an Intel NPU deployment: `npu-discover` finds
  Hugging Face models the NPU can actually run, `npu-export` exports one with `optimum-cli`,
  checks it on CPU and writes the model files, GPU twin included
  ([`8db677f`](https://github.com/fmatsos/npu/commit/8db677fbc1dea9d6019075b345f11e43e7894038),
  [`bdd564c`](https://github.com/fmatsos/npu/commit/bdd564c9a0dd1e29990daa0ad1089060505c5468))
- Two deployment guides: [Intel NPU](https://github.com/fmatsos/npu/blob/main/docs/intel-npu.md) (export, quantization, OVMS, the GPU
  twin) and [Apple Silicon](https://github.com/fmatsos/npu/blob/main/docs/apple-silicon.md) (`llama-server` on Metal, started by
  `npu serve`)
  ([`b508085`](https://github.com/fmatsos/npu/commit/b5080852760ccb126e80f690950d2b9883148c40),
  [`bdd564c`](https://github.com/fmatsos/npu/commit/bdd564c9a0dd1e29990daa0ad1089060505c5468),
  [`f61c33d`](https://github.com/fmatsos/npu/commit/f61c33d3a61d6f78c7141370c2e2f24a6fec3993))

### Changed

- **Breaking**: `npu status` prints `BACKEND  RUNTIME  INSTANCE  URL  STATE` instead of
  `BACKEND  CONTAINER  STATE`. A program reading its columns by position must be updated: the
  instance (container name or pid) is now column 3
  ([`bdd564c`](https://github.com/fmatsos/npu/commit/bdd564c9a0dd1e29990daa0ad1089060505c5468),
  [`ad5803e`](https://github.com/fmatsos/npu/commit/ad5803e6d9f30448c02f095743361a4ee18680ba))
- **Breaking**: `npu models` gains a trailing `FALLBACK` column (`-` when the model declares
  none)
  ([`bdd564c`](https://github.com/fmatsos/npu/commit/bdd564c9a0dd1e29990daa0ad1089060505c5468))
- A backend's runtime is declared in a `[runtime]` table tagged by `type` (`"docker"` or
  `"process"`), so an unsupported family is rejected by name. The `[docker]` table of earlier
  versions is still accepted and means `type = "docker"`; declaring both is rejected, naming the
  file
  ([`ad5803e`](https://github.com/fmatsos/npu/commit/ad5803e6d9f30448c02f095743361a4ee18680ba))
- Release binaries are built with thin LTO: the build takes half the time, and the binary is
  about 1.5 MB larger
  ([`2d3279e`](https://github.com/fmatsos/npu/commit/2d3279e1fc00dc230e5e887dccd4c063c550bc20))

**Full changelog**: [`v0.2.0...v0.3.0`](https://github.com/fmatsos/npu/compare/v0.2.0...v0.3.0)

## [0.2.0] - 2026-09-22

### Added

- `npu version` and `npu update` check, download, verify and install the latest GitHub release
  binary. Both run in degraded mode like `doctor`, since neither depends on the AI configuration
  ([`8003b25`](https://github.com/fmatsos/npu/commit/8003b257edbbb10e330f9fc71d11fa236e7b2252))

### Changed

- **Breaking**: a backend's `[timeouts]` table (`request_secs`, in seconds) is now honoured
  instead of being rejected as an unknown key, and the default request timeout rises from 30s to
  120s to cover a full `max_tokens` generation on a slow accelerator. `request_secs = 0` is
  rejected at load time, naming the file
  ([`e40cd5e`](https://github.com/fmatsos/npu/commit/e40cd5e6a90e9daf801f88034a22610be632137e))

### Fixed

- The `ovms` backend fixture pins the OpenVINO Model Server image to `2026.4.0` (was the floating
  `:latest` tag) and mounts a persistent `--cache_dir`, so NPU/GPU graph compilation happens once
  per model instead of on every container restart
  ([`829e8da`](https://github.com/fmatsos/npu/commit/829e8da063afafb3672a5c3f89b71fada556c091),
  [`c5b4a31`](https://github.com/fmatsos/npu/commit/c5b4a31c056bde9309a4a0023c1e84d62740f9c8))

**Full changelog**: [`v0.1.0...v0.2.0`](https://github.com/fmatsos/npu/compare/v0.1.0...v0.2.0)

## [0.1.0] - 2026-09-21

First release. `npu` runs prompts from the command line, and its command
tree, models, backends and output contracts are **configuration** — adding a
command does not require a rebuild.

### Added

- Commands are Markdown files under `.npu/commands/`, TOML frontmatter fenced
  by `---`; the file's path becomes the command name, so the CLI tree is built
  at startup from a directory scan
  ([`b36ea22`](https://github.com/fmatsos/npu/commit/b36ea220357f0633e1a6f1b269c807272d8e7d2f),
  [`e0f2c5a`](https://github.com/fmatsos/npu/commit/e0f2c5acffebb5c6e659d2cb6b2c7f3bdaca822b))
- Configuration is read from three scopes — `/etc/npu`,
  `$XDG_CONFIG_HOME/npu`, then `./.npu` — where a narrower scope replaces an
  entry of the same id rather than merging into it
  ([`09bc7e1`](https://github.com/fmatsos/npu/commit/09bc7e1a24746a72bf72ad6b7f287c494387c3d1))
- A command declares its own flags in `[args.*]`, and its prompt interpolates
  `{{ input }}`, `{{ args.x }}` and `{{ env.X }}`
  ([`3583dd6`](https://github.com/fmatsos/npu/commit/3583dd6e3bc096624c26d9feebf706b9ab810e58),
  [`98d8774`](https://github.com/fmatsos/npu/commit/98d87746cbbc2a183477cc98cc8e02ae208b709e))
- A command can declare an `[output]` contract with a JSON Schema; an answer
  that violates it exits `4` instead of reaching the caller
  ([`f5abc37`](https://github.com/fmatsos/npu/commit/f5abc37a0b656b3792ba5e72882f3a9e4c3e834f))
- `npu doctor` diagnoses a setup, `npu models` lists the declared aliases and
  `npu describe` prints what a command resolves to. Broken configuration puts
  `npu` in degraded mode: `--help` still works, every other command exits `2`
  with the offending file named
  ([`712229c`](https://github.com/fmatsos/npu/commit/712229c8cd1c2b8fb22dbb0f34abf329e3d29dc8))
- `npu serve`, `stop`, `status` and `logs` drive the container of a backend
  declaring a `[docker]` table — image, ports and accelerator come from that
  table, so switching runtime is a configuration change. Docker is optional:
  a machine that declares no container is never penalized
  ([`e0f2c5a`](https://github.com/fmatsos/npu/commit/e0f2c5acffebb5c6e659d2cb6b2c7f3bdaca822b))
- `--verbose error|warn|info`, global, default `warn`. Diagnostics go to
  stderr only; no level ever adds or removes a byte on stdout, which carries
  the command result and nothing else
  ([`e0f2c5a`](https://github.com/fmatsos/npu/commit/e0f2c5acffebb5c6e659d2cb6b2c7f3bdaca822b))
- Exit codes are the API for the programs driving `npu`: `1` I/O, `2` the
  user's configuration is wrong, `3` the backend is unreachable or answered
  non-2xx, `4` the answer violated the declared contract
  ([`b36ea22`](https://github.com/fmatsos/npu/commit/b36ea220357f0633e1a6f1b269c807272d8e7d2f),
  [`712229c`](https://github.com/fmatsos/npu/commit/712229c8cd1c2b8fb22dbb0f34abf329e3d29dc8))
- Five Claude Code skills — `npu-config`, `npu-backend`, `npu-model`,
  `npu-command`, `npu-doctor` — write and repair the configuration files an
  agent has to produce
  ([`29f7007`](https://github.com/fmatsos/npu/commit/29f7007008972f675bf14b7c19b8da6d4c431466),
  [`92af8b2`](https://github.com/fmatsos/npu/commit/92af8b2c3835a5c71c65e8960274557bcebb84aa),
  [`f7ac509`](https://github.com/fmatsos/npu/commit/f7ac50976a991dff1c378ad684a7d3cbaf280641))
- Prebuilt binaries for Linux (x86-64, ARM64), macOS (Intel, Apple silicon)
  and Windows (x86-64, ARM64), plus the Linux x86-64 triple built on Fedora
  and on Arch
  ([`03fd1bc`](https://github.com/fmatsos/npu/commit/03fd1bc19963100bfd5b18cc6ad721bc0aa64c42),
  [`61bf2dd`](https://github.com/fmatsos/npu/commit/61bf2dd3d57affe20fb17f4d7c0c89262e7eb933))

**Full changelog**: [`v0.1.0`](https://github.com/fmatsos/npu/commits/v0.1.0)

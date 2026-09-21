# Changelog

Every notable change to `npu`, newest first. Versions follow
[semantic versioning](https://semver.org); pre-1.0, a breaking change bumps
the minor.

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

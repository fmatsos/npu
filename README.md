# npu

[![QA](https://github.com/fmatsos/npu/actions/workflows/qa.yml/badge.svg)](https://github.com/fmatsos/npu/actions/workflows/qa.yml)

A generic CLI engine for running local AI commands, written in Rust, against any OpenAI-compatible
model server — OVMS on an Intel NPU, `llama-server` on Apple Silicon, or anything else that speaks
the protocol.

`npu` hard-codes no business commands. There is no `classify`, no `summarize`, no `transcribe` in
the binary. You declare your own commands as configuration files, and the CLI builds its command
tree from them at startup:

```text
npu       = generic execution engine
commands  = configuration
models    = configuration
backends  = configuration
```

Adding, changing or removing a command never requires recompiling. A repository can ship its own
`.npu/` directory and get project-specific AI tooling without shipping any executable code.

> [!NOTE]
> Not published to crates.io — grab a binary from the
> [releases](https://github.com/fmatsos/npu/releases) or build from source (see below).

---

## Table of contents

- [How it works](#how-it-works)
- [Prerequisites](#prerequisites)
- [Installation](#installation)
- [Quick start](#quick-start)
- [Built-in commands](#built-in-commands)
- [Exit codes](#exit-codes)
- [Use from an agent](#use-from-an-agent)
- [Documentation](#documentation)

---

## How it works

Four concepts, deliberately kept separate:

| Concept | Declares | Lives in |
| --- | --- | --- |
| **Command** | user intent, prompt, accepted input, CLI arguments, output contract | `commands/*.md` |
| **Model** | a concrete model id, its backend, the backend operation it uses | `models/*.toml` |
| **Backend** | runtime protocol, connection details, available operations | `backends/*.toml` |
| **Schema** | the JSON contract a structured command must satisfy | `schemas/*.json` |
| **Hardware tooling** | preparing a configuration for Intel/OpenVINO and Hugging Face (`model discover`, `backend tune`) | `src/vendor/` |

A command names a model; a model names a backend and one of its operations. The command never
needs to know which endpoint or protocol is involved.

The Rust core understands execution mechanics, not AI business semantics. `model discover` and
`backend tune` are the one deliberate exception: they know Intel/OpenVINO and Hugging Face well
enough to help prepare a configuration, but they never run a command themselves, and everything
they know lives under `src/vendor/`, never in the engine that does.

A shared `.npu/` directory cannot launch a program as a side effect of running a business
command. Runtime startup is an explicit operator action (`npu backend serve`), not part of the
business pipeline. Treat command prompts and backend endpoints from a shared repository as
untrusted configuration nonetheless.

---

## Prerequisites

**Rust toolchain.** The repository pins its toolchain in `rust-toolchain.toml`, so
[rustup](https://rustup.rs) downloads the right version (1.98.1, edition 2024) automatically. You
do not need to install a specific Rust version by hand — you only need rustup itself.

**An OpenAI-compatible backend, reachable over HTTP.** `npu` speaks the OpenAI chat-completions
protocol; every operation a backend declares is a `POST` under the hood, `chat` being the
conventional name. [OpenVINO Model
Server](https://github.com/openvinotoolkit/model_server) is the reference target, but anything
exposing `POST /v1/chat/completions` (or an equivalent path you configure) will do.

Without a backend listening, every business command fails with exit code `3`. The built-ins
(`doctor`, `config models`, `backend serve`, `describe`) still work.

**Docker — optional.** Only the lifecycle commands (`npu backend serve`, `stop`, `status`, `logs`) need it: a
backend can declare a `[runtime]` table with `type = "docker"` saying how to start its own
runtime, and those commands drive it. Nothing else in the CLI touches Docker, and a configuration
without that table never asks for it. The other family, `type = "process"`, needs no daemon at
all — it spawns the server the backend names directly on this machine.

---

## Installation

### From a release

Each `vX.Y.Z` tag publishes a stripped binary per target — `x86_64-unknown-linux-gnu`,
`aarch64-unknown-linux-gnu`, `x86_64-apple-darwin`, `aarch64-apple-darwin`,
`x86_64-pc-windows-msvc`, `aarch64-pc-windows-msvc` — on the
[releases page](https://github.com/fmatsos/npu/releases), together with the changelog for that
version. The Linux binaries link only glibc and run on any glibc-based distribution. Each platform ships
twice: an archive (`npu-vX.Y.Z-<platform>.tar.gz` or `.zip`)
containing the executable and the README, and a raw, uncompressed executable
(`npu-<platform>`, or `npu-<platform>.exe` on Windows) — the one `npu update` downloads itself,
authenticated against the SHA-256 in the release's `npu-update.json` manifest. Either way, put
`npu` (renaming the raw download and, on Linux and macOS, `chmod +x` it) anywhere on your `PATH`;
there is nothing else to install. Future releases can then be installed in place with `npu
update`, provided the directory containing the executable is writable by the current user.

### Linux and macOS

```sh
# 1. Install rustup if you don't have it
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
. "$HOME/.cargo/env"

# 2. Build and install npu
git clone <repository-url> npu
cd npu
cargo install --path .
```

`cargo install` places the binary in `~/.cargo/bin`, which rustup adds to your `PATH`.

> [!TIP]
> On a distribution that ships its own Rust package, make sure `~/.cargo/bin` comes **before**
> `/usr/bin` in your `PATH`, otherwise the system `rustc` shadows the pinned toolchain.

### Windows

```powershell
# 1. Install rustup from https://rustup.rs (rustup-init.exe)

# 2. Build and install npu
git clone <repository-url> npu
cd npu
cargo install --path .
```

> [!WARNING]
> **Windows support is partial.** Configuration scopes follow Windows conventions: the system
> scope is `%ProgramData%\npu`, the user scope `%APPDATA%\npu` (see
> [configuration](docs/configuration.md)), and the project `.npu` is found as on Unix. The runtime
> lifecycle is split: `type = "docker"` works (Docker Desktop is its prerequisite, not `npu`'s
> code), while a backend declaring `type = "process"` is
> rejected at load time naming the file — that family needs a `$XDG_STATE_HOME`/`$HOME` state
> directory and a `SIGTERM`, neither of which Windows has. Everything else — command discovery,
> arguments, templating, structured output, the other built-ins — is platform-independent.

`cargo install --path .` builds with the `hardware-tooling` Cargo feature on by default, which
declares `npu model discover` and `npu backend tune` (see [How it works](#how-it-works) and
[`docs/cli.md`](docs/cli.md#cargo-feature-hardware-tooling)). Build with `--no-default-features`
for a smaller, engine-only CLI without them.

### Verify the installation

```sh
npu doctor
```

This validates your configuration, probes every configured backend and compiles every declared
output schema. It is the fastest way to tell whether a setup problem is yours or your runtime's —
see [Exit codes](#exit-codes).

---

## Quick start

Create a `.npu/` directory in your project:

```text
.npu/
├── backends/
│   └── ovms.toml
├── models/
│   └── qwen-fast.toml
└── commands/
    └── commit-message.md
```

**`.npu/backends/ovms.toml`** — where to send requests:

```toml
id = "ovms"
type = "openai-compatible"
base_url = "http://127.0.0.1:8000"

[operations.chat]
method = "POST"
path = "/v3/chat/completions"
```

**`.npu/models/qwen-fast.toml`** — which model, on which backend operation:

```toml
id = "qwen-fast"
backend = "ovms"
operation = "chat"
model = "qwen-2.5-1.5b"

[generation]
temperature = 0.0
max_tokens = 512
```

**`.npu/commands/commit-message.md`** — the command itself. TOML frontmatter between `---`
fences, and the prompt as the body:

```markdown
---
description = "Generate a conventional commit message"
model = "qwen-fast"

[input]
mode = "stdin"

[output]
format = "text"
max_lines = 1
---

Generate a Conventional Commit message from the supplied diff.

Return exactly one commit message.

Do not use Markdown.
Do not explain your answer.

{{ input }}
```

The filename becomes the command name, so this is now a real subcommand:

```sh
git diff --cached | npu commit-message
```

Nested directories become nested subcommands: `commands/git/review.md` gives you `npu git review`.

`npu` behaves like a proper Unix tool — **stdout carries the command result and nothing else**,
diagnostics go to stderr, at a verbosity you choose (`--verbose error|warn|info`, default `warn`):

```sh
npu summarize README.md > summary.txt
cat ticket.md | npu classify | jq .
journalctl -u nginx --since -30min | npu analyze-logs
```

Commands can declare their own flags, which become real CLI arguments:

```sh
cat README.md | npu translate --language french
```

See [Writing commands](docs/commands.md) for arguments, templating and input modes.
See [Testing configured prompts](docs/testing.md) for `npu config test` cases and reports.

---

## Built-in commands

Three groups (`backend`, `config`, `model`) plus `doctor`, `describe`, `update` and `help` ship
with the binary. They are not AI commands, and their names are reserved — a command file called
`doctor.md` is rejected at load time.

```console
$ npu --version
npu 0.6.1
```

```console
$ npu update
updated npu from 0.6.0 to 0.6.1
```

```console
$ npu config models
NAME       BACKEND  OPERATION
qwen-fast  ovms     chat
```

```console
$ npu backend serve qwen-fast
2ac5416d2aae6769b9c2674ee2e284eaab4be049fa7d02d38146852989c35e35
```

```console
$ npu backend status
BACKEND  RUNTIME  INSTANCE  URL                    STATE
ovms     docker   npu-ovms  http://127.0.0.1:8000  Up Less than a second
```

```console
$ npu backend logs qwen-fast --follow
[2026-09-21 17:26:44.688][1][serving][info][server.cpp:115] OpenVINO Model Server 2026.4.0.869b2186a
```

`npu describe translate` returns a JSON command description, including each argument's type
and bounds or enum values when declared.

`npu backend serve`, `npu backend stop`, `npu backend status` and `npu backend logs` are the runtime lifecycle: start a model's
backend, end it, see what is up, read what it printed. `npu backend tune` sizes the context
and memory of NPU- and GPU-compiled models from the model and the host's RAM, and
`npu model discover` lists the Hugging Face models the host can run
(`--backend openvino|llamacpp|mlx|<backend>` for one engine, `--npu` for the Intel NPU). What gets started comes from the backend's
`[runtime]` table — a container (`type = "docker"`) or a plain local process
(`type = "process"`) — so switching family, image, command, ports or accelerator is a
configuration change, not a rebuild.

`npu doctor` reports on configuration, backend reachability and output schemas. It runs **even
when your configuration is invalid** — that is its whole point. See [Built-in
commands](docs/cli.md).

`npu --version` prints the version embedded from `Cargo.toml`. `npu update` reads the latest
release manifest from GitHub, selects the binary for the current platform, verifies its SHA-256,
then replaces the running executable at the same path. Both commands remain usable when the AI
configuration is invalid because neither depends on it.

---

## Use from an agent

Run `npu mcp serve` as a stdio MCP server. It exposes each configured business command as a
tool; built-ins are not tools. The tool name joins command path segments with `_`, and its
`inputSchema` describes typed `[args]`. Pass command input under the literal JSON property
`"mcp.input"` (distinct from a configured argument named `input`). JSON commands also return
`structuredContent`; failures return an error envelope there and set `isError`.

The server uses MCP protocol `2026-07-28` (`server/discover` and request `_meta`). It reads
configuration once: restart to see changes. If loading fails, it remains available with an
empty tool list and discovery instructions directing you to `npu doctor`. See [MCP server](docs/mcp.md).

## Exit codes

`npu` is designed to be called by other programs, including coding agents. Exit codes are a
contract, not an afterthought.

| Code | Meaning | Who should act |
| ---: | --- | --- |
| `0` | Success | — |
| `1` | I/O or update error (unreadable file, failed download/replacement) | You |
| `2` | Configuration error — your files are wrong | You: fix the named file |
| `3` | Backend error — unreachable, or an HTTP failure | Your runtime: start or fix it |
| `4` | Output contract violation — the model answered badly | Your prompt or your schema |

Every configuration error names the offending file, and where relevant the line. A malformed
structured response is an execution failure, never something `npu` quietly repairs.

---

## Documentation

| Guide | Contents |
| --- | --- |
| [Configuration](docs/configuration.md) | scopes and precedence, backends, models, merge semantics |
| [Writing commands](docs/commands.md) | command files, frontmatter, arguments, templating, input modes |
| [Output contracts](docs/output.md) | text and JSON output, JSON Schema validation, fenced responses |
| [MCP server](docs/mcp.md) | agent integration over stdio, tool naming, argument and output schemas |
| [Built-in commands](docs/cli.md) | `doctor`, `config`, `backend`, `model`, `describe`, `update`, degraded mode |
| [Deploying on an Intel NPU](docs/intel-npu.md) | exporting a model with `optimum-cli`, quantization pitfalls, serving it with OVMS |
| [Running on Apple Silicon](docs/apple-silicon.md) | serving a GGUF model with `llama-server` on Metal, started and stopped by `npu backend serve` |
| [Claude Code skills](skills/README.md) | seven skills that teach Claude Code to write and repair an `npu` configuration, find and export a model |

---

## Development

```sh
make qa       # fmt + clippy (-D warnings) + tests + cargo-deny
make fix      # apply rustfmt and clippy autofixes
make modules  # print the module structure (diagnostic only)
```

`make qa` is the single gate: rustfmt, Clippy with the `all` and `pedantic` groups as hard errors,
the full test suite, and `cargo-deny` for advisories, licences and duplicate crates. `unsafe` code
is forbidden crate-wide.

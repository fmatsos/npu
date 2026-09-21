# npu

A generic CLI engine for running local AI commands, written in Rust, with a focus on models served
by an NPU.

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
> Version 0.1.0. Not published to crates.io — install from source (see below).

---

## Table of contents

- [How it works](#how-it-works)
- [Prerequisites](#prerequisites)
- [Installation](#installation)
- [Quick start](#quick-start)
- [Built-in commands](#built-in-commands)
- [Exit codes](#exit-codes)
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

A command names a model; a model names a backend and one of its operations. The command never
needs to know which endpoint or protocol is involved.

The Rust core understands execution mechanics, not AI business semantics.

---

## Prerequisites

**Rust toolchain.** The repository pins its toolchain in `rust-toolchain.toml`, so
[rustup](https://rustup.rs) downloads the right version (1.98.1, edition 2024) automatically. You
do not need to install a specific Rust version by hand — you only need rustup itself.

**An OpenAI-compatible backend, reachable over HTTP.** `npu` speaks the OpenAI chat-completions
protocol; version 0.1.0 supports the `chat` operation only. [OpenVINO Model
Server](https://github.com/openvinotoolkit/model_server) is the reference target, but anything
exposing `POST /v1/chat/completions` (or an equivalent path you configure) will do.

Without a backend listening, every business command fails with exit code `3`. The built-ins
(`doctor`, `models`, `describe`) still work.

---

## Installation

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
> **Windows support is partial in 0.1.0.** Configuration scope resolution is written for
> Unix conventions: the system scope is the hard-coded path `/etc/npu`, and the user scope is read
> from `$HOME`, never from `%USERPROFILE%`. In practice this means only the project-local `.\.npu`
> scope works out of the box. To get a user-level scope, set `HOME` (or `XDG_CONFIG_HOME`)
> explicitly in your environment. Everything else — command discovery, arguments, templating,
> structured output, the built-ins — is platform-independent.

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

**`.npu/commands/commit-message.md`** — the command itself. TOML frontmatter between `+++`
fences, and the prompt as the body:

```markdown
+++
description = "Generate a conventional commit message"
model = "qwen-fast"

[input]
mode = "stdin"

[output]
format = "text"
max_lines = 1
+++

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
diagnostics go to stderr:

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

---

## Built-in commands

Three runtime commands ship with the binary. They are not AI commands, and their names are
reserved — a command file called `doctor.md` is rejected at load time.

```console
$ npu models
NAME       BACKEND  OPERATION
qwen-fast  ovms     chat
```

```console
$ npu describe translate
{"name":"translate","description":"Translate input text","model":"qwen-fast","input":"stdin_or_file","args":{"language":{"short":"l","required":true,"description":"Target language"}},"output":{"format":"text","schema":null,"max_lines":null}}
```

`npu doctor` reports on configuration, backend reachability and output schemas. It runs **even
when your configuration is invalid** — that is its whole point. See [Built-in
commands](docs/cli.md).

---

## Exit codes

`npu` is designed to be called by other programs, including coding agents. Exit codes are a
contract, not an afterthought.

| Code | Meaning | Who should act |
| ---: | --- | --- |
| `0` | Success | — |
| `1` | I/O error (unreadable file, broken pipe) | You |
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
| [Built-in commands](docs/cli.md) | `doctor`, `models`, `describe`, degraded mode |

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

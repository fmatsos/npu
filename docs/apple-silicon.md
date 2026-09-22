# Running on Apple Silicon

- [Overview](#overview)
- [GPU, Metal and the Neural Engine](#gpu-metal-and-the-neural-engine)
- [1. Prerequisites](#1-prerequisites)
- [2. Checking `llama-server`](#2-checking-llama-server)
- [3. The backend and the model](#3-the-backend-and-the-model)
- [4. `npu serve`, `status`, `stop`](#4-npu-serve-status-stop)
- [5. `npu doctor`](#5-npu-doctor)
- [Troubleshooting](#troubleshooting)

---

## Overview

`npu` knows nothing about Apple hardware. On a Mac it drives the same thing as anywhere else, an
OpenAI-compatible server, and the piece that uses the hardware is that server. This guide uses
[llama.cpp](https://github.com/ggml-org/llama.cpp)'s `llama-server`, started and stopped by `npu`
itself through a [process runtime](configuration.md#starting-a-backend-as-a-process), with no
container and no daemon.

```text
llama-server (Metal)  ←  npu serve / stop / status / logs
        ↑
npu <command>  →  POST /v1/chat/completions
```

Any other server speaking the protocol (an MLX server, for instance) is wired the same way: a
different `command` and `arguments`, not a different `npu`.

## GPU, Metal and the Neural Engine

An Apple Silicon chip has two accelerators, and they are not interchangeable:

- the **GPU**, programmed through **Metal**. This is what `llama-server` uses (as does MLX), and
  what this guide sets up;
- the **Neural Engine** (ANE), which is reachable only through Core ML. `llama-server` does not
  use it, and neither does anything `npu` starts today.

So, despite the name, nothing on a Mac runs on an NPU in the sense of
[the Intel guide](intel-npu.md). The model runs on the GPU, in the unified memory the CPU shares,
which is why a model has to fit in RAM with room left over for everything else.

---

## 1. Prerequisites

- An Apple Silicon Mac: `uname -m` prints `arm64`.
- The `npu` binary for `aarch64-apple-darwin`, from the
  [releases](https://github.com/fmatsos/npu/releases).
- llama.cpp. Homebrew's build has Metal enabled:

  ```sh
  brew install llama.cpp
  ```

- A model in GGUF format, for instance:

  ```sh
  hf download Qwen/Qwen2.5-1.5B-Instruct-GGUF qwen2.5-1.5b-instruct-q4_k_m.gguf --local-dir ~/models
  ```

Docker is not needed.

## 2. Checking `llama-server`

`npu serve` looks `command` up on `PATH` the way a shell does, so check it from the shell you will
run `npu` in:

```sh
command -v llama-server
llama-server --version
```

If `command -v` prints nothing, `npu serve` fails in the same way (see
[Troubleshooting](#troubleshooting)). A `llama-server` installed somewhere else can be named by
its absolute path in `command`.

---

## 3. The backend and the model

```toml
# .npu/backends/llamacpp.toml
id = "llamacpp"
type = "openai-compatible"
port = 8080
base_url = "http://127.0.0.1:{{ backend.port }}"

[operations.chat]
method = "POST"
path = "/v1/chat/completions"

[runtime]
type = "process"
command = "llama-server"
arguments = [
    "--model", "{{ env.HOME }}/models/{{ args.model }}",
    "--alias", "{{ args.model }}",
    "--host", "127.0.0.1",
    "--port", "{{ backend.port }}",
    "--n-gpu-layers", "99",
]
startup_timeout_secs = 120
```

```toml
# .npu/models/qwen-local.toml
id = "qwen-local"
backend = "llamacpp"
operation = "chat"
model = "qwen2.5-1.5b-instruct-q4_k_m.gguf"

[generation]
temperature = 0.0
max_tokens = 512
```

What each line is for:

- `port` is declared once. `base_url` and `--port` both read it as `{{ backend.port }}`, so they
  cannot drift apart. It has to be a fixed number, because a process runtime cannot hand back a
  port the kernel picked (`port = "auto"` is rejected here).
- `{{ args.model }}` is the model's `model` field. It names the file to load, and `--alias` makes
  `llama-server` answer under that same name.
- `--n-gpu-layers 99` offloads every layer to the GPU. Any number at least as large as the model's
  layer count means "all of them".
- `startup_timeout_secs` is how long `npu serve` waits for the port to answer. Loading a large
  model from disk takes a while, and 30 seconds (the default) can be too short.

Every key is described in
[Starting a backend as a process](configuration.md#starting-a-backend-as-a-process).

---

## 4. `npu serve`, `status`, `stop`

The blocks below were captured by running the binary. Pids differ from run to run, and so do the
OS error numbers: macOS reports "connection refused" as `os error 61`.

`serve` returns only once the server answers on its port, and prints its pid:

```console
$ npu serve qwen-local
1269338
```

`status` lists every backend that declares a runtime:

```console
$ npu status
BACKEND   RUNTIME  INSTANCE  URL                    STATE
llamacpp  process  1269338   http://127.0.0.1:8080  running
```

A second `serve` refuses to start a second server beside the first one (exit `3`):

```console
$ npu serve qwen-local
backend error: backend "llamacpp" is already served by process 1269338 — `npu status` to see it, `npu stop qwen-local` to end it
```

`stop` sends `SIGTERM`, then `SIGKILL` if the server does not exit in time, and prints the backend
id. It is idempotent: stopping a backend that is not running still succeeds.

```console
$ npu stop qwen-local
llamacpp
```

`npu logs qwen-local` prints what the server wrote on both streams (`-f` follows it). Its startup
lines are the quickest way to confirm that Metal was actually used:

```sh
npu logs qwen-local | grep -i -e metal -e offloaded
```

The record and the log live in `$HOME/Library/Application Support/npu/state/`. See
[What `npu` remembers](configuration.md#what-npu-remembers).

---

## 5. `npu doctor`

`doctor` checks the backend's port and, because the backend declares a process runtime, that its
`command` can be found. Before `serve`, the port is expected to fail, and that gives exit `3`:

```console
$ npu doctor
✓ configuration loaded
✗ backend "llamacpp" reachable: TCP connection to "127.0.0.1:8080" failed: Connection refused (os error 111)
✓ runtime command "llama-server" available
✓ model "qwen-local"
```

Once the server is up:

```console
$ npu doctor
✓ configuration loaded
✓ backend "llamacpp" reachable
✓ runtime command "llama-server" available
✓ model "qwen-local"
```

`doctor` connects to the port and stops there. It does not send a chat request, so a green report
does not prove that the model answers well.

---

## Troubleshooting

**`llama-server` is not on `PATH`.** `doctor` says so:

```console
✗ runtime command "llama-server" available: command "llama-server" not found on PATH — install it, or point the backend's [runtime].command at it
```

and `serve` refuses with exit `3`:

```console
$ npu serve qwen-local
backend error: backend "llamacpp": command "llama-server" not found (an absolute or relative path is used as-is, a bare name is looked up on PATH)
```

A shell that finds it while `npu` does not usually means a different `PATH`. Homebrew's prefix on
Apple Silicon is `/opt/homebrew/bin`, and a service manager does not load your shell profile.

**The port is taken.** `serve` checks before spawning anything:

```console
$ npu serve qwen-local
backend error: backend "llamacpp": port 8080 is already in use by something else — change its "port" key, or stop what is listening on it
```

Change `port` in the backend file. `base_url` and `--port` follow on their own.

**The server exits during startup.** This happens with a missing model file, an unsupported GGUF,
or not enough memory. `serve` fails with exit `3`, naming the executable, its exit status and the
log file it kept. The server's own explanation is in that file: `npu logs qwen-local`.

**`serve` times out while the model is still loading.** Raise `startup_timeout_secs`.

**The server dies when the terminal closes.** It is a child of the shell `npu serve` ran in, not
a detached daemon. Run `npu serve` from something that outlives the terminal (`nohup`, `tmux`, a
`launchd` agent) if the server has to stay up.

**`npu stop` succeeded but the port is still held.** `command` is a wrapper script that does not
end with `exec`. `npu` signalled the wrapper, and the real server survived. End the script with
`exec llama-server …`.

# Built-in commands

`npu` ships its built-in commands under seven names: three groups, `backend` (the runtime
lifecycle), `config` (inspection) and `model` (`discover`), plus `doctor`, `describe`, `update`
and `help`. They are not AI commands, and these seven names are reserved: a command file whose
first path segment is one of them is rejected at load time, naming the file. Any other name,
`status` or `logs` included, is yours.

`npu --help` lists your commands under `Commands:` and the built-ins under `Built-ins:`, `help`
included: `npu help backend serve` is `npu backend serve --help`.

On a terminal, help, reports and diagnostics are coloured. Through a pipe — or with `NO_COLOR`
set — every byte is the same as without colours: the escape sequences are stripped on the way out,
never written to a stream that is not a terminal.

A command's answer, on a terminal, is framed: a blank line, a `●` header naming the model that
actually answered (the fallback, when it took over), the answer, and a blank line. A pipe or a
file receives the answer alone, byte for byte.

A free-text answer (`format = "text"` without `max_lines`) is **streamed** to a terminal. `npu`
asks the backend for `stream: true`, and each token is printed as it arrives. Nothing is printed
until the first non-blank token. Until then the fallback can still take over, which covers a
stopped container or a prompt the NPU refuses. Once a token is on screen, the answer belongs to
that model: a failure mid-way exits `3` after a partial answer, with no fallback. A JSON answer,
or one bounded by `max_lines`, can only be validated whole, so it is never streamed. Neither is
anything written to a pipe or a file.

- [`npu doctor`](#npu-doctor) (also `npu config check`)
- [`npu config test`](testing.md)
- [`npu config models`](#npu-config-models)
- [`npu backend serve`](#npu-backend-serve)
- [`npu backend stop`](#npu-backend-stop)
- [`npu backend status`](#npu-backend-status)
- [`npu backend logs`](#npu-backend-logs)
- [`npu backend tune`](#npu-backend-tune)
- [`npu model discover`](#npu-model-discover)
- [`npu describe`](#npu-describe)
- [`npu --version`](#npu-version)
- [`npu update`](#npu-update)
- [Verbosity](#verbosity)
- [Degraded mode](#degraded-mode)

---

## `npu doctor`

Validates the runtime environment and reports on stdout. Its report *is* its result.
`npu config check` is the same command under its grouped name; `doctor` stays at the top level
because it is what you type when nothing else works.

```console
$ npu doctor
✓ configuration loaded
✗ backend "ovms" reachable: TCP connection to "127.0.0.1:8000" failed: Connection refused (os error 111)
✓ container runtime available
✓ model "qwen-fast"
✓ command "classify": model
✓ command "commit-message": model
✓ command "translate": model
✓ command "classify": output schema
```

What it checks, in order:

1. **Configuration loads** — every scope parses and merges.
2. **Each backend is reachable.**
3. **The container runtime answers** — `docker info` succeeds. This check appears **only** if at
   least one backend declares a Docker runtime; Docker is an optional prerequisite, and a
   machine that never asked for a container is never penalized for not having one.
4. **Each runtime command is runnable** — one check per distinct `command` declared by a
   [process runtime](configuration.md#starting-a-backend-as-a-process), named, and again **only**
   if at least one backend declares one. Named rather than folded into a single line for the
   family, because unlike Docker's fixed binary this one is whatever the backend's author wrote,
   and a report that did not name it would send its reader through every backend file.
5. **Each model** names a backend that exists and an operation that backend exposes.
6. **Each command** names a model that exists.
7. **Each declared output schema** exists, is readable, is valid JSON and compiles as a schema.

Step 7 is the exhaustive pass that normal execution deliberately skips: at runtime only the
invoked command's schema is compiled, so that a broken schema elsewhere cannot disable the CLI.
`doctor` is where every schema in every scope gets checked.

### Exit codes

| Code | Meaning |
| ---: | --- |
| `0` | every check passed |
| `2` | at least one **configuration** check failed (1, 5, 6, 7) |
| `3` | **only** reachability failed (2, 3, 4) |

Configuration takes priority: an unreachable backend *and* a broken configuration gives `2`.
The rationale is that a calling program can distinguish *fix your files* from *start your
runtime*. Code `3` here means the same thing it means everywhere else in the CLI — a backend
problem.

### What `doctor` deliberately does not check

The original specification's example output includes a line for NPU availability. **It is not
implemented and not displayed.** This CLI is deliberately agnostic of the inference runtime and
has no way to observe whether an NPU is present; printing a checkmark for a check that never ran
would be a lie, and a diagnostic tool is the worst possible place for one.

For the same reason, the reachability probe opens a TCP connection and closes it — it never sends
an HTTP request. A `POST` to the `chat` operation would genuinely invoke the model, which is an
unacceptable side effect for a diagnostic command. The label therefore says *reachable*, not
*available*: a socket accepted, and that is all that was established.

---

## `npu config models`

Lists configured models, sorted by name, with column widths computed from the content.
`FALLBACK` is the model retried once when this one fails with a backend error, or `-` when none is
declared — see [`fallback`](configuration.md#fallback-optional).

```console
$ npu config models
NAME                       BACKEND   OPERATION  FALLBACK
qwen2.5-coder-3b-instruct  ovms      chat       -
qwen3-8b                   ovms      chat       qwen3-8b-gpu
qwen3-8b-gpu               ovms-gpu  chat       -
```

---

## `npu backend serve`

Starts the runtime of the backend a model points at, and prints what that runtime family calls
what it started — its result, and the only thing it writes to stdout. For a
[Docker](configuration.md#starting-a-backend-with-docker) backend that is the container
identifier; for a [process](configuration.md#starting-a-backend-as-a-process) one, the pid.

```console
$ npu backend serve qwen-fast
2ac5416d2aae6769b9c2674ee2e284eaab4be049fa7d02d38146852989c35e35
```

```console
$ npu backend serve qwen-fast
1002664
```

The argument is a **model**, not a backend: a model already names exactly one backend
(`backend = "ovms"`), so there is nothing to disambiguate, and several served backends coexist
without ceremony. What gets run comes entirely from that backend's `[runtime]` table — `npu` knows
the shape of a `docker run` invocation, or how to spawn a child process, never which server you
run.

### The Docker family

The command built is:

```text
docker run -d --name npu-<backend-id> <options…> <image> <args…>
```

`-d` and `--name` are imposed by `npu`. Detached, because an attached container would write the
server's logs onto stdout, where only the result belongs. Named after the **backend**, because
that is what owns the ports: starting the same backend twice then fails on an explicit name
conflict instead of silently running a second container fighting for port 8000. That second
attempt exits `3`, with Docker's own `Conflict. The container name "/npu-ovms" is already in use`
on stderr and nothing on stdout.

### The process family

The declared `command` is spawned directly, with the declared `arguments`, its two streams
redirected into a log file beside the state record `npu` writes, and `[runtime.env]` layered over
the environment `npu` itself runs in. Serving the same backend twice is refused before anything is
spawned, naming the backend and the pid already holding it — the state record is what makes that
possible, Docker's name registry having no equivalent here.

Then, unlike the Docker family, `npu backend serve` **waits**: it polls the backend's `base_url` until
something answers, the server exits, or `startup_timeout_secs` runs out. The poll is a TCP
connection and nothing more — no byte is sent, no protocol is spoken — so a `serve` that printed a
pid means *something accepted a connection on that address*, which a server still loading its
model already does. Use `npu doctor` or the runtime's own readiness endpoint for anything
stronger; teaching this engine an HTTP readiness path would bake a protocol assumption into it.

Every failure after the spawn terminates the child and deletes the record, but **keeps the log** —
the only place the server explained itself. The
message that reaches stderr names the backend, the executable that was run, what happened — it
exited during startup with its status, or it did not answer within its budget — and the path of
that log. stdout stays empty, as it does on every failure path of every command here.

### Exit codes

| Code | Meaning |
| ---: | --- |
| `0` | the runtime started; its container identifier or pid is on stdout |
| `2` | unknown model, or its backend declares no `[runtime]` table |
| `3` | the runtime could not be brought up |

Code `3` covers every way a start fails, whichever family: `docker` missing, its daemon down or
`docker run` failing; and for a process, a `command` this machine does not have, a port already
taken, a spawn the OS refused, a server that exited during startup or one that never answered
within its budget. The port pre-check only exists for a backend that declares a `port` key: one
spelling its number directly in `base_url` and in `arguments` has nothing for `npu` to check, and
a port already held then surfaces as the server exiting during startup, with the reason in its
log. It is the same `3` as everywhere else in this CLI — a backend problem. To a
calling program, *the runtime could not be brought up* and *the backend is unreachable* call for
the same reaction.

### What `npu backend serve` deliberately does not do

For a Docker backend it does not wait for the server to be ready: `docker run -d` returns as soon
as the container is created, long before a model is loaded. Use `npu doctor`, `npu backend status`, or the
runtime's own readiness endpoint, to know when it can answer. (A process backend does wait — see
above.)

It does not detach a spawned process into its own session either, so a terminal hang-up takes it
down along with everything else in that session.

The rest of the lifecycle lives in its own commands: [`npu backend stop`](#npu-backend-stop),
[`npu backend status`](#npu-backend-status) and [`npu backend logs`](#npu-backend-logs).

---

## `npu backend stop`

Ends what `npu backend serve` started for that model's backend.

```console
$ npu backend stop qwen-fast
npu-ovms
```

```console
$ npu backend stop qwen-fast
llamacpp
```

For a Docker backend it **removes** the container rather than merely stopping it, and prints its
name: a stopped container still owns that name, so `npu backend serve` would then fail on a conflict and
the lifecycle would be a one-way trip.

For a process backend it prints the **backend identifier**, not the pid `serve` returned. By the
time `stop` answers, that pid names nothing, and a command printing a pid when it killed one and
something else when there was nothing to kill would force its caller to branch on which. The pid,
while it exists, is [`npu backend status`](#npu-backend-status)'s `INSTANCE` column. Termination escalates:
`SIGTERM`, a bounded wait, then `SIGKILL`.

Stopping a backend that was never started is not an error in either family — the command prints
the same name and exits `0`, so a script can call it without checking first. Neither is a record
whose process is already gone, or whose pid has since been recycled: that record is forgotten,
never signalled, because the pid it holds may belong to anybody by now.

Exit codes are `npu backend serve`'s: `2` for an unknown model or a backend without a `[runtime]` table,
`3` when the runtime itself refuses — including a process that survived both signals, in which
case the record is deliberately **kept**, since forgetting a running server would leave it
unreachable to `npu`.

---

## `npu backend status`

Reports the state of every backend that declares a runtime, sorted by backend, one line
each. Its report *is* its result.

```console
$ npu backend status
BACKEND   RUNTIME  INSTANCE      URL                     STATE
ovms      docker   npu-ovms      http://127.0.0.1:8000   Up 3 hours
ovms-gpu  docker   npu-ovms-gpu  http://127.0.0.1:32768  Up 3 hours
```

```console
$ npu backend status
BACKEND   RUNTIME  INSTANCE  URL                     STATE
llamacpp  process  1002664   http://127.0.0.1:18432  running
```

`RUNTIME` is the family that manages the backend — `docker` or `process`. `INSTANCE` is what that
family calls the thing it started: the container name for Docker, the pid for a process, `-` when
there is nothing running.

`URL` is the backend's resolved `base_url` — the only place a
[`port = "auto"`](configuration.md#port-optional) shows up, since Docker allocates that number and
nothing else in the CLI would reveal it. A backend whose port cannot be read back (not started,
Docker unusable) shows `-` there: a report that died on its first unreadable line would not be a
report. A **served process** backend shows the URL its own record holds — the address it was
actually started on, and the one its state was decided against, so that editing `port` without
restarting can never print a new URL beside a verdict reached on the old one.

`STATE` is whatever the runtime says. For Docker that is Docker's own wording (`Up 3 minutes`,
`Exited (0) 2 minutes ago`); for a process it is one of `running` (the pid is ours and something
answers on that URL), `unreachable` (ours, but nothing answers — starting up, wedged, or listening
elsewhere), `exited` (the pid is gone), `stale state` (the pid was recycled and now belongs to
somebody else) or `foreign state` (a live runtime recorded by another backend file — two projects
sharing an identifier in a machine-global state directory). Every family prints `not started` when
nothing was started.

`status` reports without repairing: a stale record is named as such and left alone, because a
report that silently deleted what it describes could not be run twice. `serve` and `stop` are what
clear it.

A backend the runtime cannot even be asked about — a Docker daemon that will not answer, one
unreadable state file — gets that failure as its own state and costs nothing but its own row:
`status` is a report, and a report that dies on its first unknown line is not a report. It
therefore exits `0` as long as the configuration loads, and prints its header even when no backend
declares a runtime at all.

---

## `npu backend logs`

Streams what the served runtime wrote.

```console
$ npu backend logs qwen-fast
[2026-09-21 17:26:44.688][1][serving][info][server.cpp:115] OpenVINO Model Server 2026.4.0.869b2186a
[2026-09-21 17:26:44.688][1][serving][info][server.cpp:116] OpenVINO backend 2026.4.0-22959-99c81491cc3-releases/2026/4
```

`--follow` (`-f`) keeps streaming as new lines arrive, until you interrupt it.

For a Docker backend the container's two streams are passed through untouched — its stdout on
`npu`'s stdout, its stderr on `npu`'s stderr, in the order the runtime wrote them. Capturing and
reprinting them would reorder the interleaving, and most servers log to stderr.

For a process backend both streams were already redirected, at `serve` time, into a single log
file beside the state record — interleaved there in the order the server wrote them, for the same
reason — and this command hands that file back byte for byte. It therefore still works after the
server has exited, which is exactly when its last lines matter; `--follow` is a read to end of
file and then a poll, so it also works on a server that has not written anything yet. A backend
`npu` never served has no such file: that is exit `3`, naming the backend and the path that was
looked for, with nothing on stdout.

The logs *are* this command's result. Exit codes are `npu backend serve`'s.

---

## Cargo feature: `hardware-tooling`

`npu backend tune` and `npu model discover` (below) are the one deliberate exception to "the core
understands execution mechanics, not AI business semantics": they know Intel/OpenVINO and Hugging
Face well enough to help prepare a configuration. Everything they know lives under `src/vendor/`,
never in the engine, and the whole seam is gated by a Cargo feature, `hardware-tooling`, **on by
default**.

Building with `--no-default-features` drops both commands, and the `model` group along with
`discover` (the group would otherwise be empty), from the CLI tree — `backend`'s own `about` text
drops `tune` too:

```console
$ npu --help
Usage: npu [OPTIONS] [COMMAND]

Commands:
  bank-classify   Classify the transactions of a bank statement
  chat            Everyday question, answer streamed on a terminal
  classify        Classify an input document
  code            Code assistant
  commit-message  Generate a conventional commit message
  mr-description  Generate a merge request title and description from a diff
  rewrite         Rewrite a text (email, document) in a given tone
  sort-files      Propose a folder for each file of a listing
  synthese        Synthétise un ou plusieurs fichiers en Markdown
  translate       Translate input text

Built-ins:
  backend   Manage the runtime of a model's backend: serve, stop, status, logs
  config    Inspect the configuration: check, models, test
  mcp       Expose configured commands to MCP clients
  doctor    Check the runtime environment: configuration, backend reachability, declared output schemas
  describe  Describe a command, built-in or configured, as JSON; with none given, list every command
  update    Download and install the latest npu release from GitHub
  help      Print this message or the help of the given command

Options:
  -v, --verbose <LEVEL>        Diagnostic verbosity on stderr; stdout always carries the result only [default: warn] [possible values: error, warn, info]
      --error-format <FORMAT>  Format errors on stderr as plain text or a one-line JSON envelope [default: text] [possible values: text, json]
      --config-dir <DIR>       Use this directory as the project scope instead of walking up from the current one [env: NPU_CONFIG_DIR]
  -h, --help                   Print help
  -V, --version                Print version
```

`model` stays in `builtin::RESERVED` either way — a reserved name is part of the contract, not a
feature — so a command file still cannot be placed at `commands/model/*.md`. No dependency becomes
optional: `sysinfo` also serves the process runtime and `ureq` the backend, so this is a
compile-time seam on the engine/hardware-tooling boundary, never a smaller dependency graph.

---

## `npu backend tune`

Sizes the context and memory of every model compiled for the NPU or the GPU, from the model and the
host, and writes the result. An OpenVINO NPU graph is compiled for a fixed prompt length plus a
fixed answer length: a longer prompt is refused, and a longer answer is cut. A GPU graph allocates
its KV cache on demand, and on unified memory nothing bounds it. `tune` sizes both instead of
leaving OVMS's defaults: 1024 + 128 tokens on NPU, and an unbounded cache on GPU.

```console
$ npu backend tune --help
Size the context and memory of every NPU- and GPU-compiled model from the model and the host's RAM, and write it

Usage: npu backend tune [OPTIONS]

Options:
      --npu                   Tune the NPU models only [default: NPU and GPU, same limits]
  -v, --verbose <LEVEL>       Diagnostic verbosity on stderr; stdout always carries the result only [default: warn] [possible values: error, warn, info]
      --gpu                   Tune the GPU models only
      --max-models <N|all>    How many of the tuned models run at the same time [default: all]
      --max-memory <PERCENT>  Share of the total RAM those models get together, in percent [default: 50]
      --models-dir <DIR>      Directory holding the exports [default: $HOME/models]
      --dry-run               Print the plan without writing anything
      --kv-u8                 Store GPU KV caches as u8: about twice the context per GB
  -h, --help                  Print help
```

A model is tuned when `<models-dir>/<model>/graph.pbtxt` declares `device: "NPU"` or
`device: "GPU"`. Without `--npu` or `--gpu`, both devices are tuned together with the same limits.
With `--npu` or `--gpu`, only that device is tuned, so each device can get its own limits:

```sh
npu backend tune --npu --max-memory 35
npu backend tune --gpu --max-memory 15 --max-models 1 --kv-u8
```

Each model's share is:
- `--max-memory` percent of the total RAM,
- minus the weights of the `--max-models` heaviest tuned models,
- divided by `--max-models`.

A GPU twin loads its own copy of the weights, so it counts like any other model. The plan is the
command's result:

```console
$ npu backend tune --dry-run
RAM 65.4 GB x 50% - weights 23.0 GB = 1.6 GB per model (6 of 6 NPU or GPU models at once)

model                            device model max  KV/token  prompt  answer est. memory
qwen2.5-coder-7b-instruct        NPU        32768      56KB    1536     512       5.8GB
qwen2.5-coder-7b-instruct-gpu    GPU        32768      56KB   13824    4608       5.5GB
qwen3-4b-instruct                NPU       262144     144KB    1536     512       3.7GB
qwen3-4b-instruct-gpu            GPU       262144     144KB    5376    1792       3.3GB
qwen3-8b                         NPU        40960     144KB     512     512       5.9GB
qwen3-8b-gpu                     GPU        40960     144KB    5376    1792       5.9GB

calibration: activation_tenths=62 activation_tenths_long=90 long_context=24576 step=1024
```

`--dry-run` additionally prints the calibration constants the NPU column above was computed
from (see `vendor::openvino::graph`'s module documentation for what they were measured against
and on what hardware): `activation_tenths` and `activation_tenths_long` are the tenths of
`hidden_size x layers` bytes of static buffers charged per token below and above
`long_context`, and `step` is the token granularity the context is rounded to.

What each device gets:

| Device | Context | `graph.pbtxt` |
| --- | --- | --- |
| NPU | Grows by 1024 tokens until the share or the model's `max_position_embeddings` is reached. A quarter goes to the answer. The memory estimate counts the fp16 KV cache and the graph's static buffers, calibrated on a Meteor Lake NPU so that it never under-estimates. | `MAX_PROMPT_LEN` and `MIN_RESPONSE_LEN` at the root of `plugin_config`. `NPUW_LLM_ENABLE_PREFIX_CACHING` under `DEVICE_PROPERTIES.NPU`, which reuses a repeated prompt prefix (the NPU ignores the top-level `enable_prefix_caching`). |
| GPU | The share, in whole GiB, becomes `cache_size`. The context is what that cache holds, capped by the model. A quarter goes to the answer. | `cache_size`. `max_num_seqs: 4`, since one user never needs 256 sequences in flight. `KV_CACHE_PRECISION: "u8"` in `plugin_config` with `--kv-u8`, which holds about twice the tokens per GiB; it is removed without the flag. |

Each model file gets `[generation].max_tokens` set to the answer length. Nothing is written until
every file has been computed. A file is replaced, not rewritten, so a `graph.pbtxt` created by a
container running as another user can still be updated. The next `npu backend serve` applies the
new graph.

Re-run it after every export, re-export or `--configure`, since they reset `graph.pbtxt` to the
defaults. Also re-run it after adding a model, since each model's share then shrinks. It fails
with a configuration error (`2`) when no export for the selected devices is found, naming the
directory. It also fails with `2` when a `config.json`, `openvino_model.bin` or `graph.pbtxt` is
missing or malformed, naming the file.

## `npu model discover`

Searches Hugging Face for the models this host can run, whatever runs them: CPU, GPU or NPU. It
needs no configuration, so like `doctor` it works when yours fails to load.

```console
$ npu model discover --help
Search Hugging Face for models this host can run, judged by llmfit when it is on PATH; --backend or --npu narrow the list

Usage: npu model discover [OPTIONS] [QUERY]...

Arguments:
  [QUERY]...  Words to search for (e.g. "qwen coder"); none lists the most downloaded

Options:
      --task <TASK>                   Hugging Face task the model must serve [default: text-generation]
  -v, --verbose <LEVEL>               Diagnostic verbosity on stderr; stdout always carries the result only [default: warn] [possible values: error, warn, info]
  -n, --limit <N>                     Most models listed [default: 20]
      --candidates <N>                Hugging Face results examined before filtering [default: 100]
      --max-memory <PERCENT>          Share of the total RAM one model's INT4 weights may take, in percent, for the models llmfit does not size [default: 50]
      --min-score <SCORE>             Lowest llmfit score kept, out of 100 [default: 60]
      --sort <COLUMN[:asc|desc],...>  Order by one or more columns (model, type, params, mem, license, downloads, score, fit, on); numbers default to desc, text to asc [default: downloads]
      --backend <ENGINE|ID>           Only models packaged for this engine (openvino, llamacpp, mlx), or for the engine a configured backend runs
      --npu                           --backend openvino, on a host that has an Intel NPU
  -h, --help                          Print help
```

By default, the only filter is that the model runs well on this machine. This is decided by
[llmfit](https://github.com/AlexsJones/llmfit), when its CLI is on `PATH`
(`uv tool install llmfit`) and it knows the model: a fit of `Perfect` or `Good`, and a score of at
least `--min-score`. For a model llmfit does not know, or without llmfit, the check is an INT4
estimate (half a byte per parameter, plus 20 %) against `--max-memory` percent of the RAM. A
repository that cannot be sized that way, such as a GGUF-only one llmfit does not know, is left
out. The search also asks for `--task` (Hugging Face `pipeline_tag`, `text-generation` by
default), and leaves out anything under 100M parameters: tokenizer fixtures and toys.

`--backend` narrows the list to what one inference engine serves. It accepts an engine name, or
the identifier of a configured backend: its engine is read from what its runtime starts (the
Docker image and arguments, or the process command and arguments). `openvino/model_server` or
`ovms` means `openvino`, and `llama-server` means `llamacpp`. A backend that starts neither is a
configuration error (`2`) naming it, and so is a name that is neither an engine nor a backend.

| `--backend` | Keeps only |
| --- | --- |
| `openvino` | An architecture `optimum-intel` exports for `--task`, read at run time from its own registry (`model_configs.py` on `main`), never frozen in the binary. The original weights, not an already-quantized repository (AWQ, GPTQ, FP8, etc.): `optimum-cli export --weight-format int4` starts from those. Open weights, unless `HF_TOKEN` is set (it is then sent to the Hub). |
| `llamacpp` | GGUF repositories, searched by their Hub tag and sized from the parameter count in their GGUF header. A GGUF repository llmfit lists among a model's `gguf_sources` gets that model's score and fit. |
| `mlx` | MLX repositories, searched by their Hub tag. They are already quantized: the size is read from the bit width in the name (`-4bit`, `-8bit`), 4 bits when it gives none. |

`--npu` is `--backend openvino` on a host with an Intel NPU. With no `/dev/accel/accel*`, it fails
before any request with exit `3`. It cannot be combined with another `--backend`.

A candidate that fails a filter is left out, never listed with a caveat. The report is the
result, on stdout, sorted by downloads unless `--sort` says otherwise. `--sort` takes one or more
comma-separated `column[:asc|desc]` keys, applied in order, each breaking the ties of the previous
one. A number column (`params`, `mem`, `downloads`, `score`) defaults to `desc`, and a text column
to `asc`. `fit` ranks `Perfect` before `Good`. A value shown as `-` always sorts last, and rows
equal on every key keep the downloads order. The sort runs over every model that passed the
filters, before `-n` keeps the first ones:

```sh
npu model discover qwen3 --sort score              # best score first
npu model discover qwen3 --sort fit,params:asc     # Perfect fits, smallest first
``` The `mem GB` column is llmfit's figure when it sized the
model, and `~` marks the INT4 estimate otherwise. The `score`, `fit`, `on` and `use case` columns
appear only with llmfit. On a terminal the report is coloured: model ids stand out, a `~` estimate
and a non-permissive licence are yellow, and a `Perfect` fit and a score of 75 or more are green.
Through a pipe the table is plain.

```console
$ npu model discover qwen3 instruct -n 4
model                                     type            params  mem GB license        downloads score fit     on   use case
unsloth/Qwen3-Coder-30B-A3B-Instruct-GGUF -                    -    15.6 apache-2.0         12.4M  80.8 Perfect GPU  Code generation and completion
Qwen/Qwen3-4B-Instruct-2507               qwen3             4.0B     5.8 apache-2.0          3.9M  72.3 Perfect GPU  Instruction following, chat
Qwen/Qwen3-Coder-30B-A3B-Instruct-FP8     qwen3_moe        30.5B    15.6 apache-2.0          1.1M  80.8 Perfect GPU  Code generation and completion
QuantTrio/Qwen3-VL-30B-A3B-Instruct-AWQ   qwen3_vl_moe     31.1B   ~18.6 apache-2.0          1.1M     - -       -    -
```

A Hub or registry that cannot be reached fails with exit `3`, naming the URL. This list answers
"worth trying". [`npu-export`](intel-npu.md)'s CPU check answers "actually works", and
[`npu backend tune`](#npu-backend-tune) sizes the context once the model is exported.

## `npu describe`

Prints a JSON description of a command — a configured one or a built-in — useful for humans, and
for programs driving the CLI. The path is given as words, like the command itself
(`npu describe git review`); `git/review` is accepted too. Built-ins are looked up first.

```console
$ npu describe translate
{"name":"translate","kind":"command","description":"Translate input text","model":"qwen3-8b","backend":"ovms","fallback":"qwen3-8b-gpu","source":{"file":"/home/…/npu/.npu/commands/translate.md","scope":"/home/…/npu/.npu"},"input":"stdin_or_file","args":{"language":{"short":"l","required":true,"description":"Target language"}},"output":{"format":"text","schema":null,"max_lines":null}}
```

Pipe it through `jq` to read it:

```console
$ npu describe commit-message | jq .
{
  "name": "commit-message",
  "kind": "command",
  "description": "Generate a conventional commit message",
  "model": "qwen3-8b",
  "backend": "ovms",
  "fallback": "qwen3-8b-gpu",
  "source": {
    "file": "/home/…/npu/.npu/commands/commit-message.md",
    "scope": "/home/…/npu/.npu"
  },
  "input": "stdin",
  "args": {},
  "output": {
    "format": "text",
    "schema": null,
    "max_lines": 1
  }
}
```

For a configured command, `backend` and `fallback` are resolved from its model (`null` when the
model is not configured, which `describe` reports rather than fails on), and `source` names the
file that won and the scope it came from — which is how a shadowed command is told apart from its
winner.

A built-in is described from the command tree itself, so this works even with a broken
configuration; `degraded_mode` says whether the built-in does too:

```console
$ npu describe backend serve | jq .
{
  "name": "backend/serve",
  "kind": "builtin",
  "description": "Start the runtime of the backend a model points at",
  "args": {
    "MODEL": {
      "short": null,
      "required": true,
      "description": "Identifier of the model to serve (e.g. \"qwen-fast\")"
    }
  },
  "subcommands": [],
  "degraded_mode": false
}
```

An unknown path is a configuration error listing the configured commands:

```console
$ npu describe nexistepas
configuration error: unknown command: "nexistepas" (available commands: classify, code, commit-message, synthese, translate)
```

### The index

With no argument, `npu describe` lists every describable path instead — built-in and business,
each with its own one-line description — rather than failing on a missing required argument:

```console
$ npu describe | jq .
[
  {
    "about": "Stream the logs of the runtime started for a model's backend",
    "kind": "builtin",
    "path": "backend/logs"
  },
  ...
  {
    "about": "",
    "kind": "business",
    "path": "classify"
  },
  ...
]
```

---

## `--json`

`npu doctor` (and its alias `config check`), `npu backend status` and `npu config models` also
accept `--json`: the same report, serialized instead of formatted for a terminal.

```console
$ npu doctor --json | jq .
[
  {
    "kind": "config",
    "label": "configuration loaded",
    "status": "ok"
  },
  {
    "kind": "reachability",
    "label": "backend \"ovms\" reachable",
    "status": "failed",
    "message": "TCP connection to \"127.0.0.1:8000\" failed: Connection refused (os error 111)"
  }
]
```

`kind` and `status` are the machine contract (`CheckKind` — `"config"`/`"reachability"` —, and
`"ok"`/`"failed"` with a `message` when failed): a calling agent reads those, never `label`'s text,
to tell a configuration failure (exit `2`) apart from a reachability one (exit `3`).

## `--error-format`

`--error-format <text|json>` (default `text`) changes how errors are rendered on
stderr — both `clap` usage errors and pipeline failures. The latter include `kind`,
`message`, `exit_code`, and the available family-specific fields (for example `backend`,
`status`, or `file`). It is read
from the raw command line, before `clap` parses anything, the same way `--verbose` is (a usage
error is raised by `clap` itself while parsing, before any declared argument's value could be read
back). `--help` and `--version` are never affected: they stay `clap`'s own rendering on stdout,
exit `0`, whatever this flag says.

```console
$ npu does-not-exist --error-format json
```
```json
{"kind":"usage","message":"error: unrecognized subcommand 'does-not-exist'"}
```

`error-format` is consequently a reserved argument name: a command declaring
`[args."error-format"]` is rejected at load time, naming the file.

## `--config-dir`

`--config-dir <DIR>` (or `$NPU_CONFIG_DIR`, the flag winning when both are set) names the project
scope directly, skipping the walk-up search entirely. Read from the raw command line, before
`clap` parses anything — the project scope is resolved to LOAD the configuration, before the
`clap` tree (built from it) even exists. See [Scopes and precedence](configuration.md#scopes-and-precedence)
for the walk-up itself.

```console
$ npu --config-dir /path/to/.npu config models
```

`config-dir` is consequently a reserved argument name: a command declaring `[args."config-dir"]`
is rejected at load time, naming the file.

## Shell completions

`npu` supports dynamic shell completions through `clap_complete::CompleteEnv` — no `completions`
subcommand, no reserved name: setting `COMPLETE=<shell>` makes the binary print the shell's
registration script instead of running as usual.

```console
$ eval "$(COMPLETE=bash npu)"       # bash, once per shell session (or in ~/.bashrc)
$ eval "$(COMPLETE=zsh npu)"        # zsh
$ COMPLETE=fish npu | source        # fish
```

Once registered, `<TAB>` completes business commands, built-in group names (`backend`, `config`,
...) and the global flags — never the HIDDEN top-level built-ins themselves (`doctor`, `describe`,
`update`; `npu --help` shows them in their own section, but `clap` never lists a hidden subcommand
as a completion candidate). Completion is resolved from the SAME `clap` tree `npu` itself runs
against, discovered fresh on every request, so it reflects the current `.npu/` — including
`--config-dir`/`NPU_CONFIG_DIR`.

---

## `npu --version`

Prints the program name and the release number embedded from `Cargo.toml`:

```console
$ npu --version
npu 0.7.1
```

It does not load or require a valid AI configuration.

---

## `npu update`

Checks the latest GitHub Release and installs it over the currently running executable:

```console
$ npu update
updated npu from 0.7.0 to 0.7.1
```

When no newer release exists, it reports that fact and leaves the executable untouched:

```console
$ npu update
npu 0.7.1 is already up to date
```

The release publishes a `npu-update.json` manifest. It maps every supported platform to a raw
binary and its SHA-256 checksum. The command downloads that manifest through GitHub's stable
`releases/latest` URL, compares semantic versions, selects the current platform (one Linux build
per architecture, for every glibc-based distribution), verifies the downloaded bytes, then replaces the executable
at the same path. It exits `1` without replacing anything when the manifest, download, checksum,
platform detection, or replacement fails. The executable's directory must therefore be writable
by the current user.

The manifest and the binaries come from the same GitHub release: the SHA-256 check protects
against a corrupted download, not against a compromised release. A detached signature is
planned.

Like `--version`, `update` does not depend on the AI configuration and remains available in degraded
mode. After a successful update, the **new** binary is asked whether it accepts your configuration;
if it does not, a warning on stderr points to the changelog and the documentation — the update
itself has succeeded and exits `0`.

---

## Verbosity

`--verbose <LEVEL>` (`-v`) sets what reaches **stderr**. Three levels, accepted after any
subcommand since the argument is global:

| Level | Prints |
| --- | --- |
| `error` | nothing but the failure itself, printed by the process regardless |
| `warn` | *(default)* what the engine had to work around — an unloadable scope, for instance |
| `info` | a trace: scopes read, command resolved, input size, request sent, response received |

```console
$ echo "texte" | npu classify --verbose info > /dev/null
npu: info: scopes: /home/…/npu/.npu, /home/…/.config/npu
npu: info: command "classify" -> model "qwen-fast" (backend "ovms", operation "chat") from /home/…/npu/.npu/commands/classify.md
npu: info: input: 6 characters read from stdin
npu: info: prompt rendered: 125 characters
npu: info: POST http://127.0.0.1:8000/v3/chat/completions (model "OpenVINO/Qwen3-8B-int4-ov", timeout 30 s)
```

**No level ever changes stdout.** Verbosity moves a threshold on the diagnostic stream; the result
of a command is not a diagnostic. A failure message is printed by the process itself, once, at
every level — `--verbose error` silences the engine's commentary, never the error you need.

### `--dry-run`

Every business command leaf accepts `--dry-run`: it builds the exact request `npu` would send —
url, headers, body — through the same constructor the real call uses, and prints it as JSON
instead of sending it. Nothing is written or read but the terminal: the runtime is never resolved
(a `port = "auto"` backend keeps its `{{ backend.port }}` placeholder verbatim, since resolving it
means asking Docker), only the primary model is shown (never the fallback), and header VALUES are
redacted — only their names appear.

```console
$ echo "texte" | npu classify --dry-run
{"body":{"messages":[{"content":"Classify: texte\n","role":"user"}],"model":"OpenVINO/Qwen3-8B-int4-ov"},"headers":{},"url":"http://127.0.0.1:8000/v3/chat/completions"}
```

`--dry-run` is declared only on business command leaves (`build_clap_node`), never on a built-in:
`npu doctor --dry-run` is a `clap` usage error (exit `2`, empty stdout), the same path as any other
unrecognized flag. `dry-run` is consequently a reserved argument name: a command declaring
`[args."dry-run"]` is rejected at load time, naming the file.

### `--model`

Every business command leaf also accepts `--model <ID>`, to use a model other than the command
file's own for this one call. The override replaces `spec.model` before the model is resolved and
before the input is read: an unknown id fails exactly like an unknown model in the command file
would (`Error::Config`, exit `2`, the id named in the message), and nothing is sent to the network.

```console
$ echo "texte" | npu classify --model qwen-fast --dry-run
{"body":{"messages":[{"content":"Classify: texte\n","role":"user"}],"model":"OpenVINO/Qwen3-8B-int4-ov"},"headers":{},"url":"http://127.0.0.1:8000/v3/chat/completions"}
```

`model` is consequently a reserved argument name, for the same reason as `dry-run`: a command
declaring `[args.model]` is rejected at load time, naming the file.

### Progress indicators

On a terminal, `npu` draws a spinner on stderr while it waits for a model (relabelled when the
fallback takes over) or for a process backend to start, and a progress bar while `npu update`
downloads. They are drawn only when **stderr is a terminal** and the level is above `error`:
through a pipe — how a program driving `npu` sees it — stderr receives no escape sequence and no
carriage return, and `--verbose error` means silence. Indicators never touch stdout.

The name `verbose` and the short letter `-v` are consequently reserved: a command declaring
`[args.verbose]` or `short = "v"` is rejected at load time, naming the argument.

---

## Degraded mode

A broken configuration must not leave you without the tools to diagnose it. When loading fails,
`npu` keeps the error instead of giving up, builds its command tree with the built-ins **always**
present, and adds your commands only if loading succeeded.

```console
$ npu --help
Usage: npu [OPTIONS]

Commands:
  none: the configuration failed to load; run "npu doctor"

Built-ins:
  backend   Manage the runtime of a model's backend: serve, stop, status, logs, tune
  config    Inspect the configuration: check, models, test
  mcp       Expose configured commands to MCP clients
  model     Find models for this host: discover
  doctor    Check the runtime environment: configuration, backend reachability, declared output schemas
  describe  Describe a command, built-in or configured, as JSON; with none given, list every command
  update    Download and install the latest npu release from GitHub
  help      Print this message or the help of the given command

Options:
  -v, --verbose <LEVEL>        Diagnostic verbosity on stderr; stdout always carries the result only [default: warn] [possible values: error, warn, info]
      --error-format <FORMAT>  Format errors on stderr as plain text or a one-line JSON envelope [default: text] [possible values: text, json]
      --config-dir <DIR>       Use this directory as the project scope instead of walking up from the current one [env: NPU_CONFIG_DIR]
  -h, --help                   Print help
  -V, --version                Print version
```

Exit code `0`, and on **stderr**:

```text
npu: warn: invalid configuration (configuration error: invalid TOML in
/tmp/…/.npu/backends/k.toml: TOML parse error at line 1, column 2
  |
1 | x{[
  |  ^
key with no value, expected `=`
); run "npu doctor" for details on the failed checks
```

The warning matters as much as the help itself. Help listing zero business commands with no
explanation would be its own kind of lie.

From there:

| Command | Behaviour with a broken configuration |
| --- | --- |
| `npu --help` | exit `0`, built-ins listed, warning on stderr |
| `npu doctor`, `npu config check` | exit `2`, report on stdout naming the offending file and line |
| `npu --version`, `update` | run normally; they do not depend on the configuration |
| `npu describe <built-in>` | runs normally; `npu describe <command>` exits `2` |
| `npu backend serve`, `stop`, `status`, `logs`, `npu config models` | exit `2`, stdout empty — they need the configuration that could not load |
| anything else | exit `2`, stdout empty, error on stderr |

# Built-in commands

`npu` ships its built-in commands under six names: two groups, `backend` (the runtime lifecycle)
and `config` (inspection), plus `doctor`, `describe`, `update` and `help`. They are not AI commands,
and these six names are reserved: a command file whose first path segment is one of them is
rejected at load time, naming the file. Any other name, `status` or `logs` included, is yours.

`npu --help` lists your commands under `Commands:` and the built-ins under `Built-ins:`, `help`
included: `npu help backend serve` is `npu backend serve --help`.

On a terminal, help, reports and diagnostics are coloured. Through a pipe — or with `NO_COLOR`
set — every byte is the same as without colours: the escape sequences are stripped on the way out,
never written to a stream that is not a terminal.

- [`npu doctor`](#npu-doctor) (also `npu config check`)
- [`npu config models`](#npu-config-models)
- [`npu backend serve`](#npu-backend-serve)
- [`npu backend stop`](#npu-backend-stop)
- [`npu backend status`](#npu-backend-status)
- [`npu backend logs`](#npu-backend-logs)
- [`npu backend tune`](#npu-backend-tune)
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

## `npu backend tune`

Sizes the static context of every NPU-compiled model from the model and the host, and writes it.
An OpenVINO NPU graph is compiled for a fixed prompt length plus a fixed answer length: a longer
prompt is refused, a longer answer is cut. `tune` picks both instead of leaving OVMS's defaults
(1024 + 128 tokens).

```console
$ npu backend tune --help
Size the static context of every NPU-compiled model from the model and the host's RAM, and write it (GPU twins are outside the budget)

Usage: npu backend tune [OPTIONS]

Options:
      --max-models <N|all>    How many NPU models run at the same time [default: all]
  -v, --verbose <LEVEL>       Diagnostic verbosity on stderr; stdout always carries the result only [default: warn] [possible values: error, warn, info]
      --max-memory <PERCENT>  Share of the total RAM those models get together, in percent [default: 50]
      --models-dir <DIR>      Directory holding the exports [default: $HOME/models]
      --dry-run               Print the plan without writing anything
  -h, --help                  Print help
```

A model is tuned when `<models-dir>/<model>/graph.pbtxt` declares `device: "NPU"`. Its share is
`--max-memory` percent of the total RAM, minus the weights of the `--max-models` heaviest NPU
models, divided by `--max-models`. Within that share, the context grows by 1024 tokens up to the
model's `max_position_embeddings`, and a quarter of it goes to the answer. The memory estimate
counts the fp16 KV cache and the graph's static buffers, calibrated on a Meteor Lake NPU so that it
never under-estimates. It prints the plan, which is its result:

```console
$ npu backend tune
RAM 65.4 GB x 50% - weights 11.5 GB = 7.0 GB per model (3 of 3 NPU models at once)

model                            model max  KV/token  prompt  answer est. memory
qwen2.5-coder-7b-instruct            32768      56KB    7680    2560      11.4GB
qwen3-4b-instruct                   262144     144KB    6912    2304       8.8GB
qwen3-8b                             40960     144KB    4608    1536      11.3GB
```

It writes `MAX_PROMPT_LEN` and `MIN_RESPONSE_LEN` at the root of `plugin_config` in each
`graph.pbtxt`. It also sets `[generation].max_tokens`, to the answer length, in the model file and
in its `fallback`'s. Nothing is written until every file has been computed. A file is replaced,
not rewritten, so a `graph.pbtxt` a container created as another user can still be updated. The
next `npu backend serve` recompiles the graph. GPU twins are **outside the budget**: their context
is dynamic, but on unified memory a served twin adds its own memory on top of `--max-memory`.

Re-run it after every export, re-export or `--configure`, which reset `graph.pbtxt` to the
defaults. Also re-run it after adding a model, since each model's share then shrinks. No NPU
export found is a configuration error (`2`) naming the directory. So is a missing or malformed
`config.json`, `openvino_model.bin` or `plugin_config`, naming the file.

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

---

## `npu --version`

Prints the program name and the release number embedded from `Cargo.toml`:

```console
$ npu --version
npu 0.3.1
```

It does not load or require a valid AI configuration.

---

## `npu update`

Checks the latest GitHub Release and installs it over the currently running executable:

```console
$ npu update
updated npu from 0.1.0 to 0.2.0
```

When no newer release exists, it reports that fact and leaves the executable untouched:

```console
$ npu update
npu 0.2.0 is already up to date
```

The release publishes a `npu-update.json` manifest. It maps every supported platform to a raw
binary and its SHA-256 checksum. The command downloads that manifest through GitHub's stable
`releases/latest` URL, compares semantic versions, selects the current platform (including the
Fedora and Arch Linux x86-64 builds), verifies the downloaded bytes, then replaces the executable
at the same path. It exits `1` without replacing anything when the manifest, download, checksum,
platform detection, or replacement fails. The executable's directory must therefore be writable
by the current user.

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
  config    Inspect the configuration: check, models
  doctor    Check the runtime environment: configuration, backend reachability, declared output schemas
  describe  Describe a command, built-in or configured, as JSON
  update    Download and install the latest npu release from GitHub
  help      Print this message or the help of the given command

Options:
  -v, --verbose <LEVEL>  Diagnostic verbosity on stderr; stdout always carries the result only [default: warn] [possible values: error, warn, info]
  -h, --help             Print help
  -V, --version          Print version
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

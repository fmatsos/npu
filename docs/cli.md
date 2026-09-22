# Built-in commands

`npu` ships nine built-in commands. They are not AI commands, and their names — along with
`help` — are reserved: a command file whose first path segment is one of them is rejected at load
time, naming the file.

- [`npu doctor`](#npu-doctor)
- [`npu models`](#npu-models)
- [`npu serve`](#npu-serve)
- [`npu stop`](#npu-stop)
- [`npu status`](#npu-status)
- [`npu logs`](#npu-logs)
- [`npu describe`](#npu-describe)
- [`npu version`](#npu-version)
- [`npu update`](#npu-update)
- [Verbosity](#verbosity)
- [Degraded mode](#degraded-mode)

---

## `npu doctor`

Validates the runtime environment and reports on stdout. Its report *is* its result.

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
   least one backend declares a `[docker]` table; Docker is an optional prerequisite, and a
   machine that never asked for a container is never penalized for not having one.
4. **Each model** names a backend that exists and an operation that backend exposes.
5. **Each command** names a model that exists.
6. **Each declared output schema** exists, is readable, is valid JSON and compiles as a schema.

Step 6 is the exhaustive pass that normal execution deliberately skips: at runtime only the
invoked command's schema is compiled, so that a broken schema elsewhere cannot disable the CLI.
`doctor` is where every schema in every scope gets checked.

### Exit codes

| Code | Meaning |
| ---: | --- |
| `0` | every check passed |
| `2` | at least one **configuration** check failed (1, 4, 5, 6) |
| `3` | **only** reachability failed (2, 3) |

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

## `npu models`

Lists configured models, sorted by name, with column widths computed from the content.

```console
$ npu models
NAME       BACKEND  OPERATION
qwen-fast  ovms     chat
```

---

## `npu serve`

Starts the container runtime of the backend a model points at, and prints the started container's
identifier — its result, and the only thing it writes to stdout.

```console
$ npu serve qwen-fast
2ac5416d2aae6769b9c2674ee2e284eaab4be049fa7d02d38146852989c35e35
```

The argument is a **model**, not a backend: a model already names exactly one backend
(`backend = "ovms"`), so there is nothing to disambiguate, and several containerized backends
coexist without ceremony. What gets run comes entirely from that backend's
[`[docker]` table](configuration.md#starting-a-backend-with-docker) — `npu` knows the shape of a
`docker run` invocation, never which server you run.

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

### Exit codes

| Code | Meaning |
| ---: | --- |
| `0` | the container started; its identifier is on stdout |
| `2` | unknown model, or its backend declares no `[docker]` table |
| `3` | `docker` is missing, its daemon is down, or `docker run` failed |

Code `3` for a failed start is the same `3` as everywhere else in this CLI — a backend problem. To
a calling program, *the runtime could not be brought up* and *the backend is unreachable* call for
the same reaction.

### What `npu serve` deliberately does not do

It does not wait for the server to be ready. `docker run -d` returns as soon as the container is
created, long before a model is loaded. Use `npu doctor`, `npu status`, or the runtime's own
readiness endpoint, to know when it can answer.

The rest of the lifecycle lives in its own commands: [`npu stop`](#npu-stop),
[`npu status`](#npu-status) and [`npu logs`](#npu-logs).

---

## `npu stop`

Removes the container `npu serve` started for that model's backend, and prints its name.

```console
$ npu stop qwen-fast
npu-ovms
```

It removes rather than merely stops: a stopped container still owns its name, so `npu serve`
would then fail on a conflict and the lifecycle would be a one-way trip. Stopping a backend that
was never started is not an error — the command prints the same name and exits `0`, so a script
can call it without checking first.

Exit codes are `npu serve`'s: `2` for an unknown model or a backend without `[docker]`, `3` when
the runtime itself refuses.

---

## `npu status`

Reports the state of every backend that declares a `[docker]` table, sorted by backend, one line
each. Its report *is* its result.

```console
$ npu status
BACKEND  CONTAINER  STATE
ovms     npu-ovms   not started
```

`STATE` is whatever the runtime says (`Up 3 minutes`, `Exited (0) 2 minutes ago`), or
`not started` when no such container exists. A backend the runtime cannot even be asked about
gets that failure as its state instead: `status` is a report, and a report that dies on its first
unknown line is not a report. It therefore exits `0` as long as the configuration loads, and
prints its header even when no backend is containerized.

---

## `npu logs`

Streams the container's logs.

```console
$ npu logs qwen-fast
[2026-09-21 17:26:44.688][1][serving][info][server.cpp:115] OpenVINO Model Server 2026.4.0.869b2186a
[2026-09-21 17:26:44.688][1][serving][info][server.cpp:116] OpenVINO backend 2026.4.0-22959-99c81491cc3-releases/2026/4
```

`--follow` (`-f`) keeps streaming as new lines arrive, until you interrupt it.

The container's two streams are passed through untouched — its stdout on `npu`'s stdout, its
stderr on `npu`'s stderr, in the order the runtime wrote them. Capturing and reprinting them would
reorder the interleaving, and most servers log to stderr. The logs *are* this command's result.

Exit codes are `npu serve`'s.

---

## `npu describe`

Prints a JSON description of a configured command — useful for humans, and for programs driving
the CLI.

```console
$ npu describe translate
{"name":"translate","description":"Translate input text","model":"qwen-fast","input":"stdin_or_file","args":{"language":{"short":"l","required":true,"description":"Target language"}},"output":{"format":"text","schema":null,"max_lines":null}}
```

Pipe it through `jq` to read it:

```console
$ npu describe commit-message | jq .
{
  "name": "commit-message",
  "description": "Generate a conventional commit message",
  "model": "qwen-fast",
  "input": "stdin",
  "args": {},
  "output": {
    "format": "text",
    "schema": null,
    "max_lines": 1
  }
}
```

An unknown command is a configuration error listing what is available:

```console
$ npu describe nexistepas
configuration error: unknown command: "nexistepas" (available commands: classify, commit-message, translate)
```

---

## `npu version`

Prints only the release number embedded from `Cargo.toml`, which makes it safe to capture from a
script:

```console
$ npu version
0.1.0
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

Like `version`, `update` does not depend on the AI configuration and remains available in degraded
mode.

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

The name `verbose` and the short letter `-v` are consequently reserved: a command declaring
`[args.verbose]` or `short = "v"` is rejected at load time, naming the argument.

---

## Degraded mode

A broken configuration must not leave you without the tools to diagnose it. When loading fails,
`npu` keeps the error instead of giving up, builds its command tree with the built-ins **always**
present, and adds your commands only if loading succeeded.

```console
$ npu --help
Usage: npu [COMMAND]

Commands:
  doctor    Check the runtime environment: configuration, backend reachability, declared output schemas
  models    List configured models
  serve     Start the container runtime of the backend a model points at
  stop      Stop and remove the container started for a model's backend
  status    Report the state of every containerized backend
  logs      Stream the logs of the container started for a model's backend
  describe  Describe a dynamically configured command, as JSON
  version   Print the current npu release version
  update    Download and install the latest npu release from GitHub
  help      Print this message or the help of the given subcommand(s)

Options:
  -v, --verbose <LEVEL>  Diagnostic verbosity on stderr; stdout always carries the result only [default: warn] [possible values: error, warn, info]
  -h, --help             Print help
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
| `npu doctor` | exit `2`, report on stdout naming the offending file and line |
| `npu version`, `update` | run normally; they do not depend on the configuration |
| `npu serve`, `stop`, `status`, `logs` | exit `2`, stdout empty — they need the configuration that could not load |
| anything else | exit `2`, stdout empty, error on stderr |

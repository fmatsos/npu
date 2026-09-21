---
name: npu-doctor
description: Diagnoses a broken `npu` setup — reads `npu doctor` output and the exit code contract (`1` I/O, `2` configuration, `3` backend, `4` output contract) to tell apart a wrong configuration file, an unreachable runtime and a model that answered badly, then repairs the named file. Covers degraded mode (why `npu --help` still works and every other command exits `2`), scope shadowing surprises, and the checks `doctor` deliberately does not perform.
when_to_use: >
  Trigger on "npu doesn't work", "npu fails", "why does npu exit 2 / 3 / 4",
  "npu doctor says", "my npu command is not listed", "configuration error",
  "unknown command", or any npu invocation that failed and needs to be
  diagnosed rather than written.
model: inherit
effort: high
allowed-tools: Read Write Edit Glob Grep Bash(npu:*)
---

# Diagnosing an `npu` configuration

## Start here

```sh
npu doctor
echo $?
```

`doctor` runs **even when the configuration is invalid** — that is its whole
point. Its report *is* its result, so it goes to stdout.

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

What it checks, in order: (1) every scope parses and merges; (2) each backend
is reachable; (3) the container runtime answers (`docker info`), **only** if a
backend declares a `[docker]` table — Docker is an optional prerequisite and a
machine without it is never penalized; (4) each model names a backend that
exists and an operation that backend exposes; (5) each command names a model
that exists; (6) every declared output schema exists, is readable, is valid
JSON and compiles.

Step 6 is the exhaustive pass normal execution deliberately skips — at runtime
only the invoked command's schema is compiled, so a broken schema elsewhere
cannot disable the CLI.

## Read the exit code first

| Code | Meaning | Who acts |
| ---: | --- | --- |
| `0` | success | — |
| `1` | I/O — unreadable file, broken pipe | you |
| `2` | **configuration** — your files are wrong | you: the message names the file |
| `3` | **backend** — unreachable, or a non-2xx HTTP response | your runtime: start or fix it |
| `4` | **output contract** — the model's answer violated the declaration | your prompt or your schema |

`doctor` narrows it further: `2` if any *configuration* check failed, `3` if
**only** reachability failed — the backend probe and the container runtime
check both count as reachability. Configuration takes priority — a broken config
*and* an unreachable backend gives `2`.

The `2` / `4` split is the useful one for a calling program: `2` means *your
configuration is broken*, `4` means *your configuration is fine and the model
answered badly*. A missing or malformed schema file is `2`, because the fault
is in the configuration, even though it surfaces only when the command runs.

## Degraded mode

When loading fails, `npu` keeps the error instead of giving up, builds its
tree with the built-ins **always** present, and adds your commands only if
loading succeeded.

| Command | With a broken configuration |
| --- | --- |
| `npu --help` | exit `0`, built-ins listed, warning on stderr |
| `npu doctor` | exit `2`, report on stdout naming the offending file and line |
| `npu serve` / `stop` / `status` / `logs` | exit `2`, stdout empty — they need the configuration that could not load |
| anything else | exit `2`, stdout empty, error on stderr |

So: **`npu --help` succeeding proves nothing.** If it lists only the
built-ins (`doctor`, `models`, `serve`, `stop`, `status`, `logs`, `describe`,
`help`), the configuration failed to load — read
stderr, then run `npu doctor`.

## Symptom → cause

| Symptom | Look at |
| --- | --- |
| `npu --help` lists no business command | configuration failed to load; stderr names the file |
| a command you wrote is missing from `--help` | wrong directory, not `.md`, or its first path segment is a reserved name (`doctor`, `models`, `serve`, `stop`, `status`, `logs`, `describe`, `help`) |
| `unknown command: "x" (available commands: …)` | the command was never discovered — check the path under `commands/` |
| exit `2` naming a file in `/etc/npu` you cannot edit | override it in `./.npu` with the same `id` (backends/models) or the same command path |
| a local override is ignored | replacement is keyed by `id` for backends and models, by full path for commands — a different `id` creates a second entry instead of replacing |
| a broken file in a broad scope kills everything | **parse errors on backends/models are always fatal**, even when shadowed: an unparseable file has no knowable identity, so nothing can tell whether it is shadowed. Commands are keyed by path, so a shadowed broken command file is never opened. |
| exit `3` with everything green in `doctor` | reachable socket, wrong `path` on the operation, or a non-2xx response — `doctor` never sends an HTTP request |
| `npu serve` exits `2` naming a backend | that backend declares no `[docker]` table — npu was never told how to start it |
| `npu serve` exits `3` | `docker` is missing, its daemon is down, or `docker run` failed; its own message is on stderr |
| a command file is diagnosed as having no frontmatter | the fence is `---`; a file still opening with `+++` is rejected with its own message |
| a container is running but the model does not answer | `docker run -d` returns before the model is loaded — `npu status` says `Up`, `npu logs <model>` says how far it got |
| exit `4` | the response violated `[output]`: not JSON, schema violation, or more lines than `max_lines`. Every schema violation is listed, not just the first. |

`npu` does not retry, does not reformulate, and does not ask the model again:
invalid structured output is a failure, by design, so that a calling program
gets a stable contract instead of a best effort.

## What `doctor` deliberately does not check

- **NPU availability.** The original specification's example shows such a
  line. It is not implemented and not displayed: this CLI is agnostic of the
  inference runtime and has no way to observe whether an NPU is present.
  Printing a checkmark for a check that never ran would be a lie, and a
  diagnostic tool is the worst place for one.
- **That the backend actually answers.** The probe opens a TCP connection and
  closes it. A `POST` to `chat` would genuinely invoke the model — an
  unacceptable side effect for a diagnostic. Hence *reachable*, not
  *available*: a socket was accepted, and that is all that was established.
- **That a container started by `npu serve` is ready.** `docker run -d`
  returns as soon as the container is created, long before a model is loaded.
  The container check answers "is the runtime usable", never "is the model
  loaded".

## Seeing more

`--verbose info` traces what the engine did, on stderr: scopes read, command
resolved, input size, request sent, response received. `--verbose error`
silences even the default warnings. **No level changes stdout** — a failure
message is printed once, at every level.

```sh
echo "texte" | npu classify --verbose info > /dev/null
```

## Repairing

1. Read the message — it names the offending file, and the line where
   relevant.
2. Open that file. Do not guess at keys: unknown keys are rejected on purpose,
   so a rejection means the key does not exist, not that it is misplaced.
3. Fix it with the matching skill: **npu-backend**, **npu-model**,
   **npu-command**, or **npu-config** for scope and precedence questions.
4. `npu doctor` again, until exit `0` — or exit `3` with only reachability
   failing, which is a configuration that is correct and a runtime that is not
   running.

## Reference

This skill is a summary. When a case is not covered here, or when the
behaviour it describes does not match what the binary does, the repository
documentation is authoritative:

- [Built-in commands and degraded mode](https://github.com/fmatsos/npu/blob/main/docs/cli.md)
- [Exit codes](https://github.com/fmatsos/npu/blob/main/docs/output.md#exit-codes)
- [When a broader scope is broken](https://github.com/fmatsos/npu/blob/main/docs/configuration.md#when-a-broader-scope-is-broken)

Related skills: **npu-config**, **npu-backend**, **npu-model**, **npu-command**.

<!-- model/effort: Diagnosis: reading a report, forming a hypothesis, testing it against the exit code. Inherits the session model on purpose — you chose it for the debugging session you are already in. -->

# Built-in commands

`npu` ships three runtime commands. They are not AI commands, and their names — along with
`help` — are reserved: a command file whose first path segment is one of them is rejected at load
time, naming the file.

- [`npu doctor`](#npu-doctor)
- [`npu models`](#npu-models)
- [`npu describe`](#npu-describe)
- [Degraded mode](#degraded-mode)

---

## `npu doctor`

Validates the runtime environment and reports on stdout. Its report *is* its result.

```console
$ npu doctor
✓ configuration loaded
✗ backend "ovms" reachable: TCP connection to "127.0.0.1:8000" failed: Connection refused (os error 111)
✓ model "qwen-fast"
✓ command "classify": model
✓ command "commit-message": model
✓ command "translate": model
✓ command "classify": output schema
```

What it checks, in order:

1. **Configuration loads** — every scope parses and merges.
2. **Each backend is reachable.**
3. **Each model** names a backend that exists and an operation that backend exposes.
4. **Each command** names a model that exists.
5. **Each declared output schema** exists, is readable, is valid JSON and compiles as a schema.

Step 5 is the exhaustive pass that normal execution deliberately skips: at runtime only the
invoked command's schema is compiled, so that a broken schema elsewhere cannot disable the CLI.
`doctor` is where every schema in every scope gets checked.

### Exit codes

| Code | Meaning |
| ---: | --- |
| `0` | every check passed |
| `2` | at least one **configuration** check failed (1, 3, 4, 5) |
| `3` | **only** reachability failed (2) |

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
  describe  Describe a dynamically configured command, as JSON
  help      Print this message or the help of the given subcommand(s)

Options:
  -h, --help  Print help
```

Exit code `0`, and on **stderr**:

```text
npu: invalid configuration (configuration error: invalid TOML in
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
| anything else | exit `2`, stdout empty, error on stderr |

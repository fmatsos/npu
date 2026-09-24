---
name: npu-backend
description: Writes and fixes `npu` backend files (`.npu/backends/*.toml`) — the `id`, `type`, `base_url` and `[operations.<name>]` tables that tell `npu` where to send requests and on which HTTP path, plus the optional `[runtime]` table `npu backend serve` uses to start the runtime — as a Docker container (`type = "docker"`, whose untagged `[docker]` spelling of earlier versions is still accepted) or as a local process (`type = "process"`), and the optional `[timeouts]` table that overrides the request timeout. Covers the constraints enforced at load time — `openai-compatible` is the only supported type, `POST` the only supported method, and unknown keys are rejected rather than ignored. Use it whenever a backend declaration is created, changed or rejected.
when_to_use: >
  Trigger on "add an npu backend", "point npu at my model server / OVMS /
  llama.cpp / Ollama", "change the base_url", "add an operation", "make npu
  start OVMS with Docker", "make npu launch llama-server itself", or on any npu
  error mentioning a backend id,
  `base_url`, `type`, `method`, an operation name or a `[runtime]`/`[docker]` key.
model: sonnet
effort: low
allowed-tools: Read Write Edit Glob Grep Bash(npu:*)
---

# `npu` backends

A backend declares the runtime protocol, where to reach it, and which
operations it exposes. Models point at a backend and one of its operations;
commands never see it.

## The file

```toml
# .npu/backends/ovms.toml
id = "ovms"
type = "openai-compatible"
base_url = "http://127.0.0.1:8000"

[operations.chat]
method = "POST"
path = "/v3/chat/completions"
```

The filename is free — **`id` is the identity**, and the merge key across
scopes. Name the file after the id anyway; anything else is a trap for the
next reader.

| Key | Required | Notes |
| --- | --- | --- |
| `id` | yes | merge key across scopes, and how models refer to this backend |
| `type` | yes | **`"openai-compatible"` is the only accepted value** |
| `base_url` | yes | joined with an operation's `path`; a trailing `/` is handled either way |
| `port` | no | declared once, read as `{{ backend.port }}` in `base_url` and `[runtime]` |
| `[operations.<name>]` | at least one | each needs `method` and `path` |
| `[runtime]` | no | how `npu backend serve` starts this backend; `type` picks the family — `"docker"` or `"process"` |
| `[timeouts]` | no | `request_secs` — overrides the default request timeout (120s) |
| `structured_output` | no | `true` when the server accepts `response_format: json_schema` (OVMS, `llama-server`, vLLM): a command's output schema is then sent with the request. Default `false` |
| `[headers]` | no | extra HTTP headers sent with every `chat` request; values accept only `{{ env.NAME }}` |

## What is rejected at load time

All of these are configuration errors (exit `2`) naming the file, never
silently ignored:

- any unknown key, at the top level or under `[operations.*]`;
- `type` other than `"openai-compatible"`;
- `method` other than `"POST"`;
- `[timeouts].request_secs = 0`, or any key inside `[timeouts]` other than `request_secs`;
- `port = 0`, a `port` string other than `"auto"`, a `{{ backend.port }}` with no `port` key, or a
  `port` key no placeholder reads;
- `port = "auto"` with anything other than a Docker runtime (a process cannot be asked which port
  it took), or whose `base_url` does not read `{{ backend.port }}`;
- two files in the same scope sharing an `id`;
- a `[runtime]` whose `type` is neither `"docker"` nor `"process"`, and any unknown key inside
  `[runtime]` — including a key belonging to the *other* family (`image` under
  `type = "process"`, `command` under `type = "docker"`);
- a backend declaring both `[runtime]` and the legacy `[docker]` table;
- `[runtime].startup_timeout_secs = 0` (process family), on the `[timeouts].request_secs`
  precedent;
- a `[headers]` value referencing anything other than `{{ env.NAME }}` (`{{ input }}`,
  `{{ args.* }}`, `{{ schemas.* }}` are rejected — a header cannot depend on the command run);
- a `[headers]` name that is `Content-Type`/`Content-Length` (case-insensitively; `npu` owns
  both), is not a legal HTTP token, or collides with another name once case is ignored;
- at request time, a `[headers]` value whose `{{ env.NAME }}` is undefined — resolved at
  preflight, before the input is read, naming the file and the header;
- inside `[runtime]`: a placeholder other than `{{ args.model }}` / `{{ env.NAME }}` /
  `{{ backend.port }}`, and an `id` unusable as a container name — or, for the process family, as
  a state file name (same rule: ASCII letters, digits, `_`, `.`, `-`, starting alphanumeric).

## Operation names are yours

`chat` is the only operation `npu` knows how to *drive* today, so a model must
name an operation the backend exposes and that operation must be usable as a
chat completion. Declaring `[operations.embeddings]` is allowed — nothing
breaks — but no model can consume it yet.

## Common targets

```toml
# OpenVINO Model Server
base_url = "http://127.0.0.1:8000"
[operations.chat]
method = "POST"
path = "/v3/chat/completions"
```

```toml
# llama.cpp server, Ollama, vLLM, LM Studio — the usual OpenAI path
base_url = "http://127.0.0.1:11434"
[operations.chat]
method = "POST"
path = "/v1/chat/completions"
```

Check the server's own documentation for the path; `npu` joins `base_url` and
`path` verbatim and does not probe for it.

## Starting the backend: the `[runtime]` table

Optional. Declaring it gives this backend a lifecycle — `npu backend serve <model>`,
`npu backend stop <model>`, `npu backend status`, `npu backend logs <model>`. `type` picks the family,
and each family reads its own keys.

### `type = "docker"`

Makes Docker a prerequisite for those four commands alone.

```toml
[runtime]
type = "docker"
image = "openvino/model_server:latest"
options = ["-p", "8000:8000", "-v", "{{ env.HOME }}/models:/models:rw"]
args = [
    "--source_model", "{{ args.model }}",
    "--model_repository_path", "/models",
    "--rest_port", "8000",
]
```

`options` go **before** the image, `args` **after** it — `docker run [OPTIONS]
IMAGE [ARG...]`. `npu` adds `-d` and `--name npu-<backend-id>`, nothing else. That name is how
`stop`, `status` and `logs` find the container afterwards.

Templating is the prompt engine's: `{{ args.model }}` (the served model's
`model` field, the only argument available here) and `{{ env.NAME }}`.
`{{ input }}` is rejected — `npu backend serve` reads no input.

The untagged `[docker]` table of earlier versions is still accepted and folded into `[runtime]`
with `type = "docker"` at load time — write `[runtime]` in new files, and never both, which is
rejected.

### `type = "process"`

Starts the server directly on this machine: no container, no daemon.

```toml
[runtime]
type = "process"
command = "llama-server"
arguments = [
    "--model", "{{ args.model }}",
    "--host", "127.0.0.1",
    "--port", "{{ backend.port }}",
]
startup_timeout_secs = 60

[runtime.env]
LLAMA_CACHE = "{{ env.HOME }}/.cache/llama.cpp"
```

| Key | Required | Notes |
| --- | --- | --- |
| `command` | yes | absolute/relative path used as-is, or a bare name looked up on `PATH` |
| `arguments` | no | one list entry per argument, never one string to be split |
| `[runtime.env]` | no | layered **over** `npu`'s own environment, never replacing it |
| `startup_timeout_secs` | no | readiness budget, `30` by default, `0` rejected |

Same templating, with one exception: `{{ backend.port }}` is substituted in `arguments` and in
`[runtime.env]` values, **not** in `command`.

Unlike `docker run -d`, `npu backend serve` **waits** here until the backend's `base_url` answers, the
server exits, or the budget runs out — so a `serve` that printed a pid means a server that
answers. It prints the pid; `npu backend stop` prints the backend id and escalates `SIGTERM` → `SIGKILL`.

`npu backend serve` writes a JSON state record and a `.log` file (both streams) under
`$XDG_STATE_HOME/npu/`, named `<backend id>-<digest of the backend file>` — that is how `stop`,
`status` and `logs` find the process again, Docker's name registry having no equivalent here, and
why two projects each declaring `llamacpp` get two records rather than fighting over one. The
record's `(pid, birth time)` pair is the identity check that keeps `stop` from killing a recycled
pid; the executable is
recorded but deliberately not compared, since a `command` ending on `exec` (a wrapper script, a
`uv`/`conda` shim) keeps the pid and swaps the image.

The spawned server is **not** detached into its own session: a terminal hang-up takes it down.

For OVMS on an accelerator: image `openvino/model_server:<version>-gpu` —
there is no NPU-only image, the GPU tag carries both plugins. In `options`,
`--device /dev/dri` and `--group-add <render gid>` for the GPU, plus
`--device /dev/accel` for the NPU.

**No `--target_device` in `args`** when serving a locally exported directory
with `--model_name`/`--model_path`: the device is baked into that export's
`graph.pbtxt` at `ovms --configure` time, and OVMS reads it from there. One
export serves one device — running the same model on both means two exports
(see **npu-export**, which builds the GPU twin as symlinks), two backends, two
ports and two containers, since `npu` names a container `npu-<backend-id>`.
That is also how several small models run in parallel: one backend each.

## Declaring the port once: the `port` key

A containerized backend spells its port twice — `-p` and `base_url` — and the two diverging is
this file's nastiest failure: `npu doctor` stays green (its probe reaches whatever answers on the
`base_url` port, possibly another backend) and only the real request fails, with exit `3`.

```toml
port = 8001
base_url = "http://127.0.0.1:{{ backend.port }}"

[runtime]
type = "docker"
options = ["-p", "{{ backend.port }}:8000", "..."]
```

`{{ backend.port }}` is substituted at load time in `base_url` and every `[runtime]` entry — the
only `backend.*` placeholder there is. Both halves are enforced: the placeholder without a `port`
is rejected, and a `port` nothing reads is rejected too.

`port = "auto"` hands the allocation to Docker: `{{ backend.port }}` becomes `0` in the `[runtime]`
lists (`-p 0:8000`), the kernel picks a free port, and `npu` reads it back with `docker port`.
Collision is impossible by construction — nothing is derived or guessed. The cost is that Docker
becomes a prerequisite for **executing commands** on that backend, not just for its lifecycle; a
fixed port never consults it. Two further rules, enforced at load: `"auto"` needs a Docker runtime
table, and it needs `base_url` to read `{{ backend.port }}`.

The port changes on each `npu backend serve`; `npu backend status` prints the resolved URL, and shows `-` for a
backend that is not started.

**A fixed port already in use is reported by `npu backend serve` itself** — exit `3`, naming the backend
and the port, before `docker run` is reached. Never moved automatically: a fixed number is a
decision something outside `npu` may depend on. `"auto"` is how you say it does not matter.

`serve` checks whether the backend is **already served** before it looks at the port: a running
container holds its own port, and diagnosing that as a port conflict would send the user to edit a
correct `port` key.

## Overriding the request timeout: the `[timeouts]` table

Optional. `npu`'s default (120s) is sized for a full `max_tokens` generation
on a slow accelerator; a backend that is slower still (a large model on an
NPU, for instance) overrides it:

```toml
[timeouts]
request_secs = 300
```

`request_secs` is the only key. `0`, or any other key inside the table, is
rejected at load time naming the file.

## Overriding a backend from a broader scope

Merging is **replacement**: a file in `./.npu` with the same `id` as one in
`/etc/npu` replaces it whole. Copy every field you still need — nothing is
inherited, `[runtime]` included.

## Verifying

```sh
npu doctor
```

`✓ backend "ovms" reachable` means a TCP socket was accepted, and nothing
more: the probe opens a connection and closes it, never sending an HTTP
request — a `POST` to `chat` would genuinely invoke the model. A reachable
backend on a wrong `path` therefore still reports green here and fails at
execution with exit `3`.

`✗ backend "ovms" reachable: TCP connection … failed` and no other failure
gives `npu doctor` exit code `3`: the configuration is fine, the runtime is
not started.

`✓ container runtime available` only appears when at least one backend
declares a Docker runtime; it means `docker info` succeeded. Its failure is a
reachability failure too — exit `3`, never `2`.

`✓ runtime command "llama-server" available` is its process-family twin: one check per distinct
`command`, named, emitted only when a backend declares a process runtime, and a reachability
failure as well — an absent executable is something to install, not a file to fix.

## Reference

This skill is a summary. When a case is not covered here, or when the
behaviour it describes does not match what the binary does, the repository
documentation is authoritative:

- [Backends](https://github.com/fmatsos/npu/blob/main/docs/configuration.md#backends)
- [Scopes and precedence](https://github.com/fmatsos/npu/blob/main/docs/configuration.md#scopes-and-precedence)
- [Starting a backend with Docker](https://github.com/fmatsos/npu/blob/main/docs/configuration.md#starting-a-backend-with-docker)
- [Starting a backend as a process](https://github.com/fmatsos/npu/blob/main/docs/configuration.md#starting-a-backend-as-a-process)
- [`npu doctor`](https://github.com/fmatsos/npu/blob/main/docs/cli.md#npu-doctor)
- [`npu backend serve`](https://github.com/fmatsos/npu/blob/main/docs/cli.md#npu-backend-serve)

Related skills: **npu-model**, **npu-config**, **npu-doctor**.

<!-- model/effort: Five keys and a table of operations; the constraints are enumerated above, not inferred. -->

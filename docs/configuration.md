# Configuration

- [Layout](#layout)
- [Scopes and precedence](#scopes-and-precedence)
- [Merge semantics](#merge-semantics)
- [Backends](#backends)
- [Starting a backend with Docker](#starting-a-backend-with-docker)
- [Models](#models)
- [When a broader scope is broken](#when-a-broader-scope-is-broken)

---

## Layout

Configuration is split by concern rather than kept in one monolithic file:

```text
.npu/
├── backends/
│   └── *.toml      # where to send requests, and how
├── models/
│   └── *.toml      # which model, on which backend operation
├── commands/
│   └── *.md        # the commands themselves (TOML frontmatter + prompt)
└── schemas/
    └── *.json      # JSON Schema contracts for structured output
```

Every directory is optional. A missing directory is not an error — it simply contributes nothing.

---

## Scopes and precedence

The same layout can exist at three levels. They are read from broadest to most local, and **the
most local wins**:

```text
/etc/npu                                  system-wide
      ↓
$XDG_CONFIG_HOME/npu   (or $HOME/.config/npu)    per user
      ↓
./.npu                                    per project
```

If `XDG_CONFIG_HOME` is set and non-empty it replaces the `$HOME`-derived path; it does not add to
it. On macOS, where XDG is not a native convention, the effective path is normally
`~/.config/npu`. A scope directory that does not exist is skipped silently.

> [!WARNING]
> On Windows, only `.\.npu` works out of the box. `/etc/npu` is a hard-coded Unix path, and the
> user scope is read from `$HOME`, never from `%USERPROFILE%`. Set `HOME` or `XDG_CONFIG_HOME`
> explicitly if you want a user-level scope.

This lets a repository ship its own `.npu/` with project-specific commands, model aliases and
backend overrides, without touching the machine or the user setup.

---

## Merge semantics

Merging is **replacement, not deep merge**. The replacement key is:

| Kind | Key |
| --- | --- |
| Backends | the `id` field inside the file |
| Models | the `id` field inside the file |
| Commands | the full command path (`git/review`), derived from the file path |

A backend with `id = "ovms"` defined in `./.npu` replaces the `/etc/npu` one **entirely**. A field
present in the broader definition and absent from the local one is *not* inherited — you get the
local file, whole.

Entries whose keys differ simply accumulate, so a system-wide command and a project command coexist.

Resolution happens *after* merging, so a model defined in your project can reference a backend
declared only in `/etc/npu`.

### Duplicate ids within one scope

Two files in the *same* scope declaring the same `id` is rejected, naming both paths. Across
scopes an override is the feature; within one scope it is an ambiguity resolved by filesystem
ordering, which is not a decision anyone made.

---

## Backends

A backend declares the runtime protocol, where to reach it, and which operations it exposes.

```toml
# .npu/backends/ovms.toml
id = "ovms"
type = "openai-compatible"
base_url = "http://127.0.0.1:8000"

[operations.chat]
method = "POST"
path = "/v3/chat/completions"
```

| Key | Required | Notes |
| --- | --- | --- |
| `id` | yes | the merge key, and how models refer to this backend |
| `type` | yes | `"openai-compatible"` is the only value supported in 0.1.0 |
| `base_url` | yes | joined with an operation's `path`; a trailing `/` is handled either way |
| `[operations.<name>]` | at least one | `method` and `path` |
| `[docker]` | no | how `npu serve` starts this backend — see below |

Unknown keys are rejected, with the file and line. A `type` other than `"openai-compatible"` and
a `method` other than `POST` are both rejected at load time rather than silently ignored.

### `[timeouts]` (optional)

```toml
[timeouts]
request_secs = 120
```

`request_secs` bounds a `chat` request against this backend, in seconds. Omitted, the backend
falls back to the CLI's own default (120s — enough for a full `max_tokens` generation on a slow
accelerator such as an NPU). `request_secs = 0` is rejected at load time, naming the file.

---

## Starting a backend with Docker

A backend may declare how to start its own runtime. `npu serve <model>` then runs it, and Docker
becomes a prerequisite — an **optional** one: nothing changes for a configuration without this
table.

```toml
# .npu/backends/ovms.toml, continued
[docker]
image = "openvino/model_server:latest"
options = ["-p", "8000:8000", "-v", "{{ env.HOME }}/models:/models:rw"]
args = [
    "--source_model", "{{ args.model }}",
    "--model_repository_path", "/models",
    "--rest_port", "8000",
]
```

| Key | Required | Notes |
| --- | --- | --- |
| `image` | yes | the container image to run |
| `options` | no | passed to `docker run` **before** the image: ports, volumes, devices |
| `args` | no | passed to the image **after** it: the server's own arguments |

Two lists rather than one because `docker run [OPTIONS] IMAGE [ARG...]` is the grammar; merging
them would make the position of the image implicit. `npu` adds `-d` and
`--name npu-<backend-id>` itself, and nothing else — it knows the shape of a `docker run`
invocation, never what you are running.

Every entry goes through the same templating as a prompt:

- `{{ args.model }}` — the `model` field of the model being served. It is the only argument
  available here; any other name is rejected at load time, naming the file.
- `{{ env.NAME }}` — an environment variable, required to be defined at `serve` time.
- `{{ input }}` — rejected: `npu serve` reads no input.

Declaring `[docker]` also constrains the backend's `id`, which becomes the container name: ASCII
letters, digits, `_`, `.` and `-`, starting with a letter or a digit. An `id` outside that set is
rejected — with its file named — rather than mangled into something Docker accepts.

> [!WARNING]
> Scope replacement is per **whole backend**, never field by field. A project scope that redefines
> `base_url` for `ovms` replaces the user scope's `ovms` entirely, `[docker]` included. Repeat the
> table in the local file, or `npu serve` will report that the backend declares none.

For OpenVINO Model Server specifically: `openvino/model_server:latest-gpu` is the image to use for
accelerators, with `--device /dev/dri` and `--group-add <render gid>` in `options` for the GPU, or
`--device /dev/accel` plus `--target_device NPU` in `args` for the NPU.

---

## Models

A model is the bridge between a command and a backend capability.

```toml
# .npu/models/qwen-fast.toml
id = "qwen-fast"
backend = "ovms"
operation = "chat"
model = "qwen-2.5-1.5b"

[generation]
temperature = 0.0
max_tokens = 512
```

| Key | Required | Notes |
| --- | --- | --- |
| `id` | yes | the merge key, and the name commands use |
| `backend` | yes | must match a backend `id` |
| `operation` | yes | must be an operation that backend exposes |
| `model` | yes | the concrete model identifier sent to the backend |
| `[generation]` | no | `temperature`, `max_tokens`; omitted fields are not sent at all |

`generation` values are only included in the request when present — no `null` is ever serialised
for an absent field.

A model naming an unknown backend, or an operation its backend does not expose, produces a
configuration error listing what *is* available.

---

## When a broader scope is broken

A broken file in `/etc/npu` must not disable your project. `/etc` may belong to root and be out of
your reach, which is exactly the case a local override is meant to solve. So a broadly-scoped
entry that is **entirely shadowed** by a more local one does not break anything.

The two paths are **deliberately asymmetric**, and unifying them would reintroduce a bug:

| | Where the key lives | Consequence |
| --- | --- | --- |
| Backends, models | the `id` field, **inside** the file | An unparseable file has no knowable identity, so there is no way to tell whether it is shadowed. **A parse error is always fatal.** Only semantic validation (`type`, `method`) is deferred until after the merge, and applies to survivors only. |
| Commands | the **file path** | The winner is known before anything is read, so only winning files are parsed. A broken but shadowed command file is never opened. |

The same reasoning governs output schemas: a schema that is missing or malformed on a command
nobody invokes does not break `npu --help`. Checking every schema in every scope is the job of
[`npu doctor`](cli.md#npu-doctor).

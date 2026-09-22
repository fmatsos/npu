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
| `port` | no | the listening port, declared once and read as `{{ backend.port }}` — see below |
| `[operations.<name>]` | at least one | `method` and `path` |
| `[docker]` | no | how `npu serve` starts this backend — see below |

Unknown keys are rejected, with the file and line. A `type` other than `"openai-compatible"` and
a `method` other than `POST` are both rejected at load time rather than silently ignored.

### `port` (optional)

A containerized backend writes its port twice — in `-p` and in `base_url` — and the two silently
diverging is the worst failure this file has: `npu doctor` stays green (its probe reaches whatever
answers on the `base_url` port, quite possibly another backend) and only the real request fails,
with exit `3`. `port` removes the second spelling:

```toml
port = 8001
base_url = "http://127.0.0.1:{{ backend.port }}"

[docker]
options = ["-p", "{{ backend.port }}:8000", "..."]
```

`{{ backend.port }}` is substituted at load time in `base_url` and in every `[docker]` entry. It is
the only `backend.*` placeholder that exists, and the two halves are enforced together: the
placeholder without a `port` key is rejected, and a `port` key nothing references is rejected too —
a value read and then ignored is exactly what this configuration does not do.

```toml
port = "auto"
```

`"auto"` hands the allocation to Docker: `{{ backend.port }}` becomes `0` in the `[docker]` lists
(`-p 0:8000`), the kernel picks a free port, and `npu` reads it back with `docker port` whenever it
needs the URL. **Collision is impossible by construction** — nothing is derived or guessed, so
there is no second candidate to try.

Deriving a port instead (a hash of the id) would be stateless but can collide with an unrelated
service; probing for a free one at each invocation does not even agree with itself, since by the
time a command runs the port is occupied — by us. Asking Docker what it allocated is the only
variant that is both collision-free and reproducible across processes, `npu` having no state to
write the answer down in.

> [!IMPORTANT]
> `port = "auto"` makes Docker a prerequisite for **executing commands** on that backend, not just
> for its lifecycle. A fixed port never consults Docker at all, so this is strictly opt-in per
> backend. Two further constraints, both rejected at load time naming the file: `"auto"` requires a
> `[docker]` table (there is nothing to read a port back from otherwise), and it requires
> `base_url` to read `{{ backend.port }}` (the allocated port would be unreachable otherwise).

The port changes on each `npu serve`. `npu status` prints the resolved URL, and a backend that is
not started reports `-` there rather than failing the report.

### When a fixed port is already taken

`npu serve` checks before starting anything and stops with exit `3`, naming the backend and the
port:

```console
$ npu serve m
backend error: backend "probe": port 8001 is already in use by something else — change its "port" key, stop what is listening on it, or use port = "auto" to let Docker allocate one
```

Deliberately not silent, and deliberately not automatic: a fixed number is a decision — something
outside `npu` connects to it, or a firewall rule names it — so moving it behind your back would
break whatever depended on it. `"auto"` is how you say the number does not matter.

The backend already being served is checked **first**, because from the outside the two look
identical and only one of them is about the port — a container holds its own port, and sending its
user to edit a `port` key that is perfectly correct is the wrong repair:

```console
$ npu serve qwen3-8b
backend error: backend "ovms" is already served by container "npu-ovms" — `npu status` to see it, `npu stop qwen3-8b` to remove it
```

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

For OpenVINO Model Server specifically: the `-gpu` image tag is the one to use for accelerators
(there is no NPU-only image; that tag carries both plugins), with `--device /dev/dri` and
`--group-add <render gid>` in `options` for the GPU, plus `--device /dev/accel` for the NPU.

When the served directory is a local export addressed with `--model_name`/`--model_path`, no
device flag belongs in `args`: the device is baked into that export's `graph.pbtxt` by
`ovms --configure`, and OVMS reads it from there. One export therefore serves one device, so
running the same weights on the NPU and the GPU means two backends on two ports — which is also
what lets several small models run at once.

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
| `fallback` | no | another model `id` to retry against when this one fails — see below |
| `[generation]` | no | `temperature`, `max_tokens`; omitted fields are not sent at all |

`generation` values are only included in the request when present — no `null` is ever serialised
for an absent field.

A model naming an unknown backend, or an operation its backend does not expose, produces a
configuration error listing what *is* available.

### `fallback` (optional)

```toml
id = "qwen3-8b"
backend = "ovms"        # OVMS on the NPU
model = "qwen3-8b-int4-ov"
fallback = "qwen3-8b-gpu"
```

When the request fails with exit code `3` (a backend failure: unreachable, or a non-2xx answer),
the same rendered prompt is sent once to the fallback model, on its own backend. Nothing else is
retried — a `2` (bad configuration) and a `4` (the answer violated the output contract) are
returned as-is, because retrying elsewhere would only hide them.

This exists for a concrete case: a model compiled for an Intel NPU has a static maximum prompt
length, and OVMS refuses an over-long prompt with a clean `400 ... Input length exceeds the
maximum allowed length` in milliseconds. That is an exact, cheap signal that the prompt belongs on
a GPU-served model instead — no token counting on `npu`'s side, no guessed character threshold.

Three properties worth knowing:

- **The retry is single hop.** The fallback's own `fallback` is not followed, so a chain cannot
  form and no cycle is possible.
- **It is blind to the reason.** `npu` cannot tell an over-long prompt from a stopped container,
  so the primary failure is always written to stderr at `warn` level. Without it, a backend that
  has been down all day would look like a healthy fallback.
- **It is checked at load time.** A `fallback` naming an unknown model, or naming its own model,
  is a configuration error (exit `2`) naming the file — not a surprise on the day the recovery is
  actually needed.

When both fail, the error names both models and both backends. `npu models` shows the `FALLBACK`
column so the routing is never invisible.

> [!IMPORTANT]
> **The fallback does not lift the model's context length.** A GPU twin built by symlinking the
> primary's export shares its `config.json`, hence its context length (40960 tokens for
> `qwen3-8b`). The fallback recovers prompts sitting between the NPU's compiled shape and that
> ceiling; past it both fail, with two distinct messages — `Input length exceeds the maximum
> allowed length` is the NPU's static shape, `Number of prompt tokens: N exceeds model max length:
> M` is the model's context, and only the first one is recoverable.

> [!NOTE]
> The NPU and the GPU need two separate backends, hence two containers and two ports: the target
> device is baked into the served export (OVMS reads it from `graph.pbtxt`), not chosen per
> request. The same is true of running several small models at once — one backend each. See
> [Starting a backend with Docker](#starting-a-backend-with-docker).

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

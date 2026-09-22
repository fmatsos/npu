# Configuration

- [Layout](#layout)
- [Scopes and precedence](#scopes-and-precedence)
- [Merge semantics](#merge-semantics)
- [Backends](#backends)
- [Starting a backend with Docker](#starting-a-backend-with-docker)
- [Starting a backend as a process](#starting-a-backend-as-a-process)
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
| `[runtime]` | no | how `npu serve` starts this backend — see below |

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

[runtime]
type = "docker"
options = ["-p", "{{ backend.port }}:8000", "..."]
```

`{{ backend.port }}` is substituted at load time in `base_url` and in every `[runtime]` entry that
can be resolved before the runtime exists: `image`, `options` and `args` for a Docker runtime,
`arguments` and the `[runtime.env]` values for a process one. **Not** a process runtime's
`command` — an executable whose *name* depends on a port is not a case worth a substitution, and
leaving it out means a `port` read only there is reported as unused rather than silently dropped.

It is the only `backend.*` placeholder that exists, and the two halves are enforced together: the
placeholder without a `port` key is rejected, and a `port` key nothing references is rejected too —
a value read and then ignored is exactly what this configuration does not do.

```toml
port = "auto"
```

`"auto"` hands the allocation to Docker: `{{ backend.port }}` becomes `0` in the `[runtime]` lists
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
> Docker runtime (`[runtime]` with `type = "docker"`, there is nothing to read a port back from
> otherwise), and it requires `base_url` to read `{{ backend.port }}` (the allocated port would be
> unreachable otherwise).

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

A backend may declare how to start its own runtime, in a `[runtime]` table whose `type` picks the
**family**: `"docker"` (below) or `"process"` (see
[Starting a backend as a process](#starting-a-backend-as-a-process)). Each family reads its own
keys, and a key belonging to the other one is rejected by name — the table is tagged precisely so
that `npu` never has to guess which shape it is looking at.

`npu serve <model>` then runs it, and the family's prerequisite — Docker here — becomes an
**optional** one: nothing changes for a configuration without this table.

```toml
# .npu/backends/ovms.toml, continued
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

| Key | Required | Notes |
| --- | --- | --- |
| `type` | yes | `"docker"`, which selects this family |
| `image` | yes | the container image to run |
| `options` | no | passed to `docker run` **before** the image: ports, volumes, devices |
| `args` | no | passed to the image **after** it: the server's own arguments |

`type` is what makes an unsupported family a named rejection — `unknown variant "podman"`, with
the file — instead of a table `npu` would have to guess the meaning of. Unknown keys inside
`[runtime]` are rejected like everywhere else, and that includes a key of the *other* family:
`image` under `type = "process"` is a mistake worth naming, not one to ignore.

### The legacy `[docker]` table

Earlier versions spelled this table `[docker]`, untagged. That spelling is **still accepted**: it
is folded into `[runtime]` with `type = "docker"` at load time, once, so nothing downstream can
tell which form a file used. `[runtime]` is the form to write in new files.

A backend declaring **both** is rejected at load time, naming the file and the backend: picking a
winner would mean reading one table and silently ignoring the other.

Two lists rather than one because `docker run [OPTIONS] IMAGE [ARG...]` is the grammar; merging
them would make the position of the image implicit. `npu` adds `-d` and
`--name npu-<backend-id>` itself, and nothing else — it knows the shape of a `docker run`
invocation, never what you are running.

Every entry goes through the same templating as a prompt:

- `{{ args.model }}` — the `model` field of the model being served. It is the only argument
  available here; any other name is rejected at load time, naming the file.
- `{{ env.NAME }}` — an environment variable, required to be defined at `serve` time.
- `{{ input }}` — rejected: `npu serve` reads no input.

Declaring a Docker runtime also constrains the backend's `id`, which becomes the container name: ASCII
letters, digits, `_`, `.` and `-`, starting with a letter or a digit. An `id` outside that set is
rejected — with its file named — rather than mangled into something Docker accepts.

> [!WARNING]
> Scope replacement is per **whole backend**, never field by field. A project scope that redefines
> `base_url` for `ovms` replaces the user scope's `ovms` entirely, `[runtime]` included. Repeat
> the table in the local file, or `npu serve` will report that the backend declares none.

For OpenVINO Model Server specifically: the `-gpu` image tag is the one to use for accelerators
(there is no NPU-only image; that tag carries both plugins), with `--device /dev/dri` and
`--group-add <render gid>` in `options` for the GPU, plus `--device /dev/accel` for the NPU.

When the served directory is a local export addressed with `--model_name`/`--model_path`, no
device flag belongs in `args`: the device is baked into that export's `graph.pbtxt` by
`ovms --configure`, and OVMS reads it from there. One export therefore serves one device, so
running the same weights on the NPU and the GPU means two backends on two ports — which is also
what lets several small models run at once.

---

## Starting a backend as a process

The other runtime family starts a server **directly on this machine**, with no container and no
daemon: `llama.cpp`'s `llama-server`, an MLX server, a shell script of your own. Same table, same
`npu serve` / `stop` / `status` / `logs`, different `type`.

```toml
# .npu/backends/llamacpp.toml, continued
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
| `type` | yes | `"process"`, which selects this family |
| `command` | yes | an absolute or relative path used as-is, or a bare name looked up on `PATH`; templated like the rest |
| `arguments` | no | the server's own arguments, one list entry per argument |
| `[runtime.env]` | no | variables **layered over** the environment `npu` itself runs in |
| `startup_timeout_secs` | no | readiness budget in seconds, `30` by default; `0` is rejected |

`arguments` is a list of separate entries, never one string to be split: a model path containing a
space would otherwise become two arguments, and there is no shell here to blame it on. Entries go
through the same templating as a Docker runtime's — `{{ args.model }}`, `{{ env.NAME }}`,
`{{ backend.port }}` — with `{{ input }}` rejected, since `npu serve` reads no input.

`command` is templated too — `{{ args.model }}` and `{{ env.NAME }}`, so a server living under a
path only the environment knows can be named — but **not** `{{ backend.port }}`: an executable
whose path depends on a port is not a case this supports, and the placeholder is rejected there
naming the file. `npu doctor` emits no "runtime command available" check for a templated
`command`: it has no model to resolve it against, and reporting the template itself as a missing
binary would tell its reader to install `{{ env.LLAMA_BIN }}`.

The lookup requires an **executable** file. A regular file with no execute bit is skipped and the
`PATH` scan continues, exactly as a shell does — so a non-executable leftover early on `PATH`
cannot shadow the real server, nor make `npu doctor` green about a command `npu serve` then
refuses to spawn.

`[runtime.env]` is an **overlay**, not a replacement: the child inherits `npu`'s own environment
and these values are layered on top. A server needing `HOME`, `PATH` or a proxy setting therefore
does not have to redeclare them to gain one variable.

`startup_timeout_secs` is what `npu serve` waits, having spawned the server, for it to answer on
its `base_url` — so a `base_url` this family cannot parse into a host and a port is rejected at
load time naming the file: a probe that can never succeed would burn the whole budget and then
terminate a perfectly working server, blaming a timeout key that was correct. The budget's failure
message carries the last probe error for the same reason, so "connection refused" and "the port in
`base_url` is not the one `arguments` gave the server" do not look alike. Unlike `docker run -d`, this family does **not** return before the server is ready:
a `serve` that succeeded means something answered. `0` is rejected at load time naming the file,
on the `[timeouts].request_secs` precedent — honoured literally it would make every start fail,
and clamped it would be a key read and then ignored.

Two constraints this family adds, both rejected at load time naming the file:

- `port = "auto"` is refused. Docker can be asked which port it allocated; a process cannot, so
  there would be nothing to read the answer back from. Declare a fixed `port`.
- the backend `id` must be usable as a file name (ASCII letters, digits, `_`, `.` and `-`,
  starting with a letter or a digit) — the same rule the Docker family applies to a container
  name, here because `npu` derives this backend's state file from the identifier.
- `startup_timeout_secs` must be between `1` and `86400`. The upper bound is not taste: a larger
  value cannot be turned into a deadline at all, and a `serve` that panicked would replace this
  CLI's exit codes with `101`.

This family is **Unix-only**. On Windows a `[runtime] type = "process"` backend is rejected at
load time naming the file: there is no `$XDG_STATE_HOME`/`$HOME` convention to put the state
record under, and no `SIGTERM` — `stop`'s graceful step would silently collapse into an immediate
hard kill, with no chance for a server to flush. Use `type = "docker"` there, or start the server
outside `npu`.

### What `npu` remembers

Docker is its own registry, so a Docker runtime needs nothing persisted. A process has no
registry: `npu serve` therefore writes a small JSON record named after the backend, plus a `.log`
file it redirects the server's **two** streams into. `stop` deletes the record; `logs` reads the
file. They live in `$XDG_STATE_HOME/npu/`, or `$HOME/.local/state/npu/` when that variable is
unset — and on macOS in `$HOME/Library/Application Support/npu/state/`, with no `XDG_STATE_HOME`
branch at all: the variable has no meaning there, and honouring it would scatter one machine's
state over two places depending on which shell exported what.

That directory is **machine-global** while backend identifiers are per-scope, so two projects each
declaring `llamacpp` in their own `./.npu` do land on the same record. The record therefore also
holds the backend **file** it was served from: one naming another file, while its process is
alive, is reported as `foreign state` and is never signalled, never cleared and never written
over — `serve` and `stop` both refuse, naming both files. Once nothing is behind that pid the
record describes nothing, and the next `serve` simply forgets it.

The record holds the pid and the moment the system says that pid was born. That **pair** is the
identity check, and it is what keeps `npu stop` from killing an innocent process: a pid alone can,
after a reboot or enough process churn, name somebody else's. A record whose pid was recycled is
reported as `stale state` and forgotten — never signalled. The birth is an epoch **second**, which
is the resolution of the check: two processes sharing a pid and born inside the same second are
indistinguishable to it. Reaching that needs the pid space to wrap within one second — a container
with a small `pid_max` namespace, not a stock machine — and there is no finer token without
`unsafe`.

The executable is recorded too, but only so that whoever reads the file knows what was started. It
is deliberately **not** compared: a `command` ending on `exec` — a wrapper script, a virtualenv or
`uv`/`conda` shim — replaces the running image while keeping the pid and its birth, so comparing it
would declare `npu`'s own child an impostor.

Changing a served backend's `[runtime]` family — or removing the table — leaves that record
unreachable: `stop`, `status` and `logs` dispatch on what the files say **today**, and a backend
that now declares Docker is asked about a container. Run `npu stop` before changing the family.
The record is plain JSON and holds the pid, so a forgotten one is still recoverable by hand.

> [!WARNING]
> The spawned server is a plain child of the shell `npu serve` ran in. It is **not** detached into
> its own session, so a terminal hang-up takes it down with everything else in that session. Run
> `npu serve` from a session that outlives it (a service manager, `nohup`, a multiplexer) if the
> server is meant to stay up.

> [!WARNING]
> `npu` signals the process it spawned, and only that one. A `command` whose process **is** the
> server — a binary, or a launcher ending on `exec` — is stopped correctly. A launcher that forks
> and waits instead (`sh -c "server | tee log"`, `conda run`, anything that does not `exec`) has
> its wrapper signalled while the real server survives: `npu stop` reports success and deletes the
> record, a failed `npu serve` terminates the wrapper and abandons the rest, and the orphan keeps
> the port while every `npu` command reports the backend as never started. There is no process
> group to signal instead without `unsafe`, so end your launcher on `exec`.

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

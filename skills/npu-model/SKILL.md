---
name: npu-model
description: Writes and fixes `npu` model files (`.npu/models/*.toml`) — the `id`, `backend`, `operation`, `model` and optional `[generation]` fields that bridge a command to a backend capability. Covers resolution errors (unknown backend, operation the backend does not expose), the fact that omitted generation fields are not sent at all, and replacement-by-id across configuration scopes. Use it whenever a model alias is created, renamed, retuned or rejected.
when_to_use: >
  Trigger on "add an npu model", "point this command at another model",
  "change temperature / max_tokens", "npu models", or on any npu error
  mentioning a model id, an unknown backend, or an operation a backend does
  not expose.
model: sonnet
effort: low
allowed-tools: Read Write Edit Glob Grep Bash(npu:*)
---

# `npu` models

A model is the bridge between a command and a backend capability. The command
names a model; the model names a backend and one of its operations. The
command never needs to know the endpoint or the protocol.

## The file

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
| `id` | yes | merge key across scopes, and the name commands use |
| `backend` | yes | must match a backend `id` |
| `operation` | yes | must be an operation that backend exposes |
| `model` | yes | the concrete model identifier sent to the backend |
| `[generation]` | no | `temperature`, `max_tokens` |

`id` and `model` are different things on purpose: `id` is the stable alias
your commands reference, `model` is whatever the server happens to call the
weights today. Swapping the weights is then a one-line edit that no command
sees.

## `[generation]` — omitted means absent

A field left out of `[generation]` is **not serialised at all**; no `null` is
ever sent, and the backend's own default applies. Set `temperature = 0.0`
explicitly when determinism matters — for a command whose output is parsed by
another program, that is usually what you want.

## What is rejected at load time

Configuration errors (exit `2`), each naming what *is* available:

- `backend` naming a backend that does not exist;
- `operation` the named backend does not expose;
- any unknown key, at the top level or under `[generation]`;
- two files in the same scope sharing an `id`.

Resolution runs **after** the scopes are merged, so a model in `./.npu` may
reference a backend declared only in `/etc/npu`. A model whose file is
replaced by a more local one with the same `id` is replaced whole — no field
is inherited.

## Naming

Name models by what the caller needs, not by the vendor: `qwen-fast`,
`classifier`, `long-context`. A command reading `model = "fast"` survives the
day the weights change; one reading `model = "qwen-2.5-1.5b-instruct-q4"` does
not.

## Verifying

```sh
npu models    # NAME / BACKEND / OPERATION, sorted by name
npu doctor    # resolves every model against its backend and operation
```

`npu models` lists what actually resolved. A model you just wrote and cannot
see there was not loaded — `npu doctor` will say why and name the file.

## Reference

This skill is a summary. When a case is not covered here, or when the
behaviour it describes does not match what the binary does, the repository
documentation is authoritative:

- [Models](https://github.com/fmatsos/npu/blob/main/docs/configuration.md#models)
- [Merge semantics](https://github.com/fmatsos/npu/blob/main/docs/configuration.md#merge-semantics)
- [`npu models`](https://github.com/fmatsos/npu/blob/main/docs/cli.md#npu-models)

Related skills: **npu-backend**, **npu-command**, **npu-doctor**.

<!-- model/effort: Five keys and two optional generation fields; resolution errors name what is available. -->

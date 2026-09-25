---
name: npu-model
description: Writes and fixes `npu` model files (`.npu/models/*.toml`) — the `id`, `backend`, `operation`, `model`, optional `fallback` and optional `[generation]` fields that bridge a command to a backend capability. Covers resolution errors (unknown backend, operation the backend does not expose), the single-hop `fallback` retry that moves an over-long prompt from an NPU-served model to a GPU-served one, the fact that omitted generation fields are not sent at all, and replacement-by-id across configuration scopes. Use it whenever a model alias is created, renamed, retuned or rejected.
when_to_use: >
  Trigger on "add an npu model", "point this command at another model",
  "change temperature / max_tokens", "npu config models", or on any npu error
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
| `model` | yes | the concrete model identifier sent to the backend, and what `{{ args.model }}` substitutes in a `[docker]` table |
| `fallback` | no | another model `id`, retried once on a backend failure |
| `[generation]` | no | `temperature`, `max_tokens`, `seed`, `top_p`, `stop`, `[generation.extra]` |

A model whose operation has `protocol = "embeddings"` takes no `fallback` and
no `[generation]`, and a fallback must speak the same protocol as its model
(rejected at load time). Its commands declare `format = "json"`: the answer
is the vector.

`id` and `model` are different things on purpose: `id` is the stable alias
your commands reference, `model` is whatever the server happens to call the
weights today. Swapping the weights is then a one-line edit that no command
sees.

## `[generation]` — omitted means absent

A field left out of `[generation]` is **not serialised at all**; no `null` is
ever sent, and the backend's own default applies. Set `temperature = 0.0`
explicitly when determinism matters — for a command whose output is parsed by
another program, that is usually what you want.

`seed` (integer) and `top_p` (float) follow the same rule. `stop` is a
non-empty list of non-empty strings, no upper bound. `[generation.extra]` is
a free-form table forwarded VERBATIM at the top level of the request, after
the typed keys — the escape hatch for an engine-specific knob
(`chat_template_kwargs.enable_thinking = false` on Qwen3). A key of `extra`
colliding with a typed key, or carrying a TOML datetime or a non-finite float
anywhere in its structure, is rejected at load time naming the file.

A command's own frontmatter may declare `[generation]` too: it MERGES onto
the model's, key by key, command winning — the one field-by-field merge in
the project (`docs/configuration.md` documents it as the explicit
exception). The fallback model uses its own base `[generation]` merged with
that same command override.

## `fallback` — one retry, one hop

```toml
fallback = "qwen3-8b-gpu"
```

An `Error::Backend` (exit `3`) on this model sends the same rendered prompt
once to the named model, on its own backend. Exit `2` and exit `4` are never
retried.

Written for the NPU case: an NPU-compiled graph has a static maximum prompt
length, and OVMS refuses an over-long prompt with a clean `400 ... Input
length exceeds the maximum allowed length` in milliseconds — an exact signal,
so `npu` needs no tokenizer and no guessed character threshold.

- **Single hop**: the fallback's own `fallback` is not followed, so no chain
  and no cycle.
- **Blind to the reason**: an unreachable container and an over-long prompt
  are the same variant, so the primary's failure is always logged at `warn` on
  stderr. A backend down all day must not pass for a healthy fallback.
- **Two devices means two backends**: the target device is baked into the
  served export (OVMS reads it from `graph.pbtxt`), never chosen per request,
  and `npu` names its container `npu-<backend-id>`. So the fallback points at
  a second model on a second backend on a second port. Same reasoning for
  running several small models at once.
- **It does not lift the context length**: a GPU twin built by symlinking the
  primary's export shares its `config.json`, hence its context window. Two
  distinct 400s therefore exist — `Input length exceeds the maximum allowed
  length` (the NPU's compiled shape, recoverable) and `Number of prompt
  tokens: N exceeds model max length: M` (the model's context, recoverable by
  nothing here).

## What is rejected at load time

Configuration errors (exit `2`), each naming what *is* available:

- `backend` naming a backend that does not exist;
- `operation` the named backend does not expose;
- `fallback` naming a model that does not exist, or naming this model itself
  — checked at load, not the day the recovery fires;
- any unknown key, at the top level or under `[generation]`;
- `[generation].stop` present but empty, or containing an empty string;
- a `[generation.extra]` key colliding with a typed `[generation]` key, or
  carrying a TOML datetime or a non-finite float anywhere in its structure;
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
npu config models    # NAME / BACKEND / OPERATION, sorted by name
npu doctor    # resolves every model against its backend and operation
npu backend serve <id>  # starts the backend's runtime, when it declares [docker]
npu backend status      # its state afterwards
```

`npu config models` lists what actually resolved. A model you just wrote and cannot
see there was not loaded — `npu doctor` will say why and name the file.

## Reference

This skill is a summary. When a case is not covered here, or when the
behaviour it describes does not match what the binary does, the repository
documentation is authoritative. The exact set of keys the binary accepts is
`npu config schema model` (a JSON Schema derived from the parser itself):

- [Models](https://github.com/fmatsos/npu/blob/main/docs/configuration.md#models)
- [Merge semantics](https://github.com/fmatsos/npu/blob/main/docs/configuration.md#merge-semantics)
- [`npu config schema`](https://github.com/fmatsos/npu/blob/main/docs/cli.md#npu-config-schema)
- [`npu config models`](https://github.com/fmatsos/npu/blob/main/docs/cli.md#npu-config-models)

Related skills: **npu-backend**, **npu-command**, **npu-doctor**.

<!-- model/effort: Five keys and two optional generation fields; resolution errors name what is available. -->

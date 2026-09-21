---
name: npu-backend
description: Writes and fixes `npu` backend files (`.npu/backends/*.toml`) — the `id`, `type`, `base_url` and `[operations.<name>]` tables that tell `npu` where to send requests and on which HTTP path. Covers the constraints that are enforced at load time: `openai-compatible` is the only supported type, `POST` the only supported method, unknown keys are rejected rather than ignored, and `[timeouts]` is not implemented. Use it whenever a backend declaration is created, changed or rejected.
when_to_use: >
  Trigger on "add an npu backend", "point npu at my model server / OVMS /
  llama.cpp / Ollama", "change the base_url", "add an operation", or on any
  npu error mentioning a backend id, `base_url`, `type`, `method` or an
  operation name.
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
| `[operations.<name>]` | at least one | each needs `method` and `path` |

## What is rejected at load time

All of these are configuration errors (exit `2`) naming the file, never
silently ignored:

- any unknown key, at the top level or under `[operations.*]`;
- `type` other than `"openai-compatible"`;
- `method` other than `"POST"`;
- a `[timeouts]` section — **not implemented**; the request timeout is a fixed
  30 seconds. It appears in the original specification but rejecting it is
  deliberate: accepting a timeout and not honouring it would be worse.
- two files in the same scope sharing an `id`.

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

## Overriding a backend from a broader scope

Merging is **replacement**: a file in `./.npu` with the same `id` as one in
`/etc/npu` replaces it whole. Copy every field you still need — nothing is
inherited.

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

## Reference

This skill is a summary. When a case is not covered here, or when the
behaviour it describes does not match what the binary does, the repository
documentation is authoritative:

- [Backends](https://github.com/fmatsos/npu/blob/main/docs/configuration.md#backends)
- [Scopes and precedence](https://github.com/fmatsos/npu/blob/main/docs/configuration.md#scopes-and-precedence)
- [`npu doctor`](https://github.com/fmatsos/npu/blob/main/docs/cli.md#npu-doctor)

Related skills: **npu-model**, **npu-config**, **npu-doctor**.

<!-- model/effort: Four keys and a table of operations; the constraints are enumerated above, not inferred. -->

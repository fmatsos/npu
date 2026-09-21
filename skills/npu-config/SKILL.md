---
name: npu-config
description: Sets up and maintains an `npu` configuration directory — the `.npu/` layout (backends, models, commands, schemas), scope precedence between `/etc/npu`, `$XDG_CONFIG_HOME/npu` and `./.npu`, and how entries from different scopes replace one another. Use it to bootstrap a project's `.npu/` from nothing, to decide which scope a piece of configuration belongs in, or to understand why a local file is (or is not) overriding a broader one. Delegates the file formats themselves to npu-backend, npu-model and npu-command.
when_to_use: >
  Trigger on "set up npu in this project", "add npu configuration", "create a
  .npu directory", "where should this npu config live", "why is my local npu
  config not winning", "npu scopes", or any npu request that spans more than
  one of backends / models / commands.
allowed-tools: Read Write Edit Glob Grep Bash(npu:*)
---

# Configuring `npu`

`npu` has no business commands compiled in. Everything — commands, models,
backends, output schemas — is configuration read at startup. Adding a command
never requires rebuilding the binary.

## The layout

```text
.npu/
├── backends/*.toml   # where to send requests, and how      → npu-backend
├── models/*.toml     # which model, on which backend op     → npu-model
├── commands/*.md     # the commands themselves              → npu-command
└── schemas/*.json    # JSON Schema contracts for JSON output
```

Every directory is optional. A missing directory contributes nothing; it is
not an error.

## Scopes and precedence

The same layout may exist at three levels, read broadest first, **most local
wins**:

```text
/etc/npu                                        system-wide
      ↓
$XDG_CONFIG_HOME/npu  (or $HOME/.config/npu)    per user
      ↓
./.npu                                          per project
```

`XDG_CONFIG_HOME`, when set and non-empty, **replaces** the `$HOME`-derived
path rather than adding to it. A scope directory that does not exist is
skipped silently.

On Windows only `.\.npu` works out of the box: `/etc/npu` is a hard-coded Unix
path and the user scope is read from `$HOME`, never `%USERPROFILE%`.

### Which scope for what

| Put it in | When |
| --- | --- |
| `./.npu` | anything specific to this repository — commit it, teammates get the tooling |
| `$XDG_CONFIG_HOME/npu` | your personal backend, your machine's model aliases |
| `/etc/npu` | a shared machine-wide backend, managed by whoever owns the box |

## Merge semantics — replacement, never deep merge

| Kind | Replacement key |
| --- | --- |
| Backends | the `id` field inside the file |
| Models | the `id` field inside the file |
| Commands | the full command path (`git/review`), derived from the file path |

A backend with `id = "ovms"` in `./.npu` replaces the `/etc/npu` one
**entirely**. A field present in the broader file and absent from the local
one is *not* inherited. Entries whose keys differ accumulate.

Resolution happens *after* merging, so a model defined in your project may
reference a backend declared only in `/etc/npu`.

Two files in the **same** scope declaring the same `id` is an error naming
both paths. Across scopes an override is the feature; within one scope it is
ambiguity resolved by filesystem ordering, which is nobody's decision.

## Bootstrapping a project

1. `mkdir -p .npu/{backends,models,commands}` (add `schemas/` when a command
   needs structured output).
2. Declare the backend → **npu-backend**.
3. Declare a model on one of its operations → **npu-model**.
4. Write the first command → **npu-command**.
5. `npu doctor` — it validates the whole chain and names whatever is broken.
   If it reports a failure, → **npu-doctor**.

Do not invent keys. Every unknown key is **rejected** at load time, by design:
a key read and silently ignored is a defect, not a shortcut. When unsure of a
key, check the matching skill rather than guessing.

## Verifying

```sh
npu doctor          # configuration, backend reachability, every declared schema
npu models          # the models actually resolved
npu describe <cmd>  # one command's effective definition, as JSON
npu --help          # the command tree built from the configuration
```

`npu --help` listing a command is proof it was discovered and parsed.

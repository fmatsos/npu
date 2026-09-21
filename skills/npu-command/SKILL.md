---
name: npu-command
description: Writes and fixes `npu` command files (`.npu/commands/*.md`) — the Markdown file whose TOML frontmatter is fenced by three dashes (the `+++` of earlier versions is rejected) and whose path becomes the CLI command name. Covers frontmatter keys, input modes, `[args.*]` flags, the three prompt placeholders (`{{ input }}`, `{{ args.x }}`, `{{ env.X }}`), the `[output]` contract with JSON Schema, reserved command names, and the load-time rejections that catch a typo before it silently reaches the model.
when_to_use: >
  Trigger on "add an npu command", "write a prompt for npu", "add a flag to
  this command", "make this command return JSON", "nest npu commands", or on
  any npu error naming a command file, a placeholder, an argument, a short
  letter, or an `[output]` key.
model: sonnet
effort: medium
allowed-tools: Read Write Edit Glob Grep Bash(npu:*)
---

# `npu` commands

A command is a Markdown file: TOML frontmatter between `---` fences (not
`+++`, which is rejected at load time with a message saying so), then the
prompt as the body. There is no registration step — **the path under
`commands/` is the command name**.

| File | Command |
| --- | --- |
| `commands/classify.md` | `npu classify` |
| `commands/commit-message.md` | `npu commit-message` |
| `commands/git/review.md` | `npu git review` |

Intermediate levels are created automatically and commands sharing a prefix
merge under the same parent. Running an intermediate level alone (`npu git`)
is a usage error: its help goes to **stderr**, exit `2`.

`doctor`, `models`, `serve`, `stop`, `status`, `logs`, `describe` and `help`
are reserved and rejected at load
time — but **on the first path segment only**, so `commands/git/describe.md`
is fine.

## The file

```markdown
---
description = "Translate input text"
model = "qwen-fast"

[args.language]
short = "l"
required = true
description = "Target language"

[input]
mode = "stdin_or_file"
---

Translate the following text into {{ args.language }}.

Preserve meaning and tone.

{{ input }}
```

```sh
cat README.md | npu translate --language french
```

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `description` | string | `""` | shown in `npu --help` |
| `model` | string | **required** | must match a model `id` |
| `[input] mode` | string | `"stdin"` | `stdin`, `file`, `stdin_or_file` |
| `[args.<name>]` | table | none | becomes a real CLI flag |
| `[output]` | table | text, no limit | see below |

**Unknown keys are rejected** — at the top level, under `[input]`, under
`[args.*]` and under `[output]`. A typo like `moed = "file"` would otherwise
fall back to the default in silence, and `npu summarize README.md` would read
stdin instead of your file without a word.

## Input modes

| Mode | Behaviour |
| --- | --- |
| `stdin` | read standard input to EOF |
| `file` | read the positional `FILE` argument; omitting it is an error |
| `stdin_or_file` | use `FILE` when given, otherwise read stdin |

Modes accepting a file get an optional positional `FILE` in their generated
CLI.

## Arguments

```toml
[args.language]
short = "l"          # exactly one character, never "-"
required = true
description = "Target language"
```

The table key is the long flag (`--language`) and the value lands in
`{{ args.language }}`. Rejected at load time: a `short` longer than one
character (never silently truncated), `short = "-"`, two arguments sharing a
short letter, a name that is empty, contains a space, starts with `-` or uses
characters no placeholder could reference, the reserved names `help`,
`version`, `FILE` and `verbose`, and the short letters `-h` (clap's help) and
`-v` (the global `--verbose`).

Default values, repeated flags and boolean flags do not exist yet.

## Prompt templating

Three placeholders, no conditions, no loops, no expressions, no includes.

| Placeholder | Resolves to |
| --- | --- |
| `{{ input }}` | the resolved input (stdin or file) |
| `{{ args.name }}` | a declared argument's value |
| `{{ env.NAME }}` | an environment variable |

Whitespace inside the braces is free. Substitution is never re-applied to
substituted content, so an argument whose value contains `{{ input }}` passes
through untouched. An environment variable **set but empty** is legitimate and
renders empty; **unset** is an error.

Two constraints worth knowing before writing a prompt:

- **A closed placeholder that is not recognised is a load-time error**, never
  copied through. `{{ args.langauge }}` would otherwise reach the model as
  literal text and the model would answer something plausible — the most
  expensive failure mode available, because it is invisible. The consequence:
  a prompt cannot contain `{{ foo }}` as literal text. An unclosed `{{` is
  left alone.
- **An argument referenced by the prompt must be `required = true`.** The
  prompt cannot be rendered without it, so declaring it optional contradicts
  the file. It is rejected rather than silently promoted.

## Output contract

```toml
[output]
format = "json"                        # "text" (default) or "json"
schema = "schemas/classification.json" # JSON only, relative to the SCOPE ROOT
```

```toml
[output]
format = "text"
max_lines = 1                          # text only
```

- `schema` resolves against the **scope root** (`schemas/` is a sibling of
  `commands/`), never against the command file or the current directory. The
  command's depth makes no difference. An absolute path is used as-is.
- `format = "json"` without a schema is allowed: the response is then only
  checked for being well-formed JSON.
- A Markdown fence wrapping the whole response is stripped; one in the middle
  is content and is left alone. stdout gets the compact serialisation of the
  parsed value, so it is always valid JSON.
- `max_lines` exceeded is a **failure** (exit `4`), never a silent truncation.
- Rejected at load: `schema` with `format = "text"`, `max_lines` with
  `format = "json"`, any unknown key.

The schema is compiled only when the command actually runs, so a broken schema
on a command nobody invokes does not break the rest of the CLI. `npu doctor`
is what checks them all.

## Ordering, and why it matters

```text
resolve command → collect arguments → check placeholders resolve
                → read input → render prompt → call backend → apply output contract
```

Everything knowable without the input is checked **before** the input is read.
Otherwise `git diff | npu commit-message` would drain the whole diff before
failing on an unset environment variable — work lost, and a non-replayable
input lost for good.

## Verifying

```sh
npu --help              # the command appears = it was discovered and parsed
npu describe <command>  # its effective definition, as JSON
npu <command> --help    # the generated flags
```

## Reference

This skill is a summary. When a case is not covered here, or when the
behaviour it describes does not match what the binary does, the repository
documentation is authoritative:

- [Writing commands](https://github.com/fmatsos/npu/blob/main/docs/commands.md)
- [Output contracts](https://github.com/fmatsos/npu/blob/main/docs/output.md)
- [`npu describe`](https://github.com/fmatsos/npu/blob/main/docs/cli.md#npu-describe)

Related skills: **npu-model**, **npu-config**, **npu-doctor**.

<!-- model/effort: Writing a prompt, choosing an input mode and declaring an output contract need judgement, and a templating mistake is only caught at load time. -->

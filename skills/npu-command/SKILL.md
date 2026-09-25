---
name: npu-command
description: Writes and fixes `npu` command files (`.npu/commands/*.md`) — the Markdown file whose TOML frontmatter is fenced by three dashes (the `+++` of earlier versions is rejected) and whose path becomes the CLI command name. Covers frontmatter keys, input modes, `[args.*]` flags, the five prompt placeholders (`{{ input }}`, `{{ args.x }}`, `{{ env.X }}`, `{{ schemas.id }}`, `{{ partials.id }}`), the `[output]` contract with JSON Schema, reserved command names, and the load-time rejections that catch a typo before it silently reaches the model.
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

After changing a prompt, add a regression case under
`.npu/tests/<command path>/<case>.toml` and run `npu config test`. See
`docs/testing.md` for the case format and report contract.

| File | Command |
| --- | --- |
| `commands/classify.md` | `npu classify` |
| `commands/commit-message.md` | `npu commit-message` |
| `commands/git/review.md` | `npu git review` |

Intermediate levels are created automatically and commands sharing a prefix
merge under the same parent. Running an intermediate level alone (`npu git`)
is a usage error: its help goes to **stderr**, exit `2`.

`backend`, `config`, `doctor`, `describe`, `update` and `help`
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
| `system` | string | none | system-role message, sent before examples and the body |
| `[[examples]]` | array of tables | none | fixed `{ user, assistant }` turns, sent in file order |

**Unknown keys are rejected** — at the top level, under `[input]`, under
`[args.*]` and under `[output]`. A typo like `moed = "file"` would otherwise
fall back to the default in silence, and `npu summarize README.md` would read
stdin instead of your file without a word.

`system` and `[[examples]]` (both optional) build the request as `[system?] +
examples×[user, assistant] + [user: rendered body]`; a command declaring
neither sends exactly the single-message body it always has. Both are
templated like the body, EXCEPT `{{ input }}`, rejected there at load time
(the input is the final user turn, rendered separately). An argument
referenced only from `system`/an example must still be `required = true`,
same rule as the body. `system` cannot be blank; an example needs non-empty
`user` and `assistant`.

```toml
system = "You are a deterministic classifier. Answer with JSON only."

[[examples]]
user = "ticket: printer on fire"
assistant = '{"category":"hardware","confidence":0.98}'
```

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
`{{ args.language }}`. Set `type = "enum"` with a non-empty distinct `values` array,
`type = "integer"` with optional inclusive `min`/`max`, or `type = "file"`
to substitute the named UTF-8 file's content. The default type is `string`.
Only enum accepts `values`; only integer accepts bounds. CLI validation fails
before execution. MCP receives file content directly.

Rejected at load time: a `short` longer than one
character (never silently truncated), `short = "-"`, two arguments sharing a
short letter, a name that is empty, contains a space, starts with `-` or uses
characters no placeholder could reference, the reserved names `help`,
`version`, `FILE` and `verbose`, and the short letters `-h` (clap's help) and
`-v` (the global `--verbose`).

Default values, repeated flags and boolean flags do not exist yet.

## Prompt templating

Five placeholders, no conditions, no loops, no expressions; the only include
is a partial, inserted verbatim.

| Placeholder | Resolves to |
| --- | --- |
| `{{ input }}` | the resolved input (stdin or file) |
| `{{ args.name }}` | a declared argument's value |
| `{{ env.NAME }}` | an environment variable |
| `{{ schemas.id }}` | a schema declared in `[schemas]`, as compact JSON |
| `{{ partials.id }}` | a text fragment declared in `[partials]`, verbatim |

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

A fragment shared between commands (style guide, glossary) goes in a partial
instead of being copied into each file:

```toml
[partials]                             # ids usable as {{ partials.<id> }}
style = "style-guide"                  # NAME -> <scope root>/partials/style-guide.md,
                                       # or a path relative to the scope root, or absolute
```

Same resolution and laziness as `[schemas]`: an undeclared id fails at load
time; a missing, non-UTF-8 file fails with exit `2` when the command runs,
naming both files. A partial is inserted verbatim and may not contain
`{{ ... }}` itself. To use a different fragment in a project, override the
command, not the partial file.

## Output contract

```toml
[output]
format = "json"                        # "text" (default) or "json"
schema = "classification"              # JSON only: a NAME (-> <scope root>/schemas/classification.json),
                                       # a path relative to the SCOPE ROOT, or an absolute path
```

When the backend declares `structured_output = true`, the output schema is sent
to the model as `response_format` and constrains its answer: the prompt does
not need to describe the JSON shape. Without it, the prompt MUST describe the
shape — `npu` only validates the answer afterwards.

```toml
[schemas]                              # ids usable as {{ schemas.<id> }} in the prompt
ticket = "ticket"                      # same three forms as [output].schema
```

An undeclared `{{ schemas.x }}` is rejected at load time.

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
- `extract = "/category"` (JSON only, a pointer starting with `/`) writes
  that one value instead of the document — a string bare — after the whole
  document passed the schema; a pointer the answer lacks is exit `4`. CLI
  stdout only: MCP and `npu config test` see the whole document.
- Rejected at load: `schema` with `format = "text"`, `max_lines` with
  `format = "json"`, `extract` with `format = "text"`, any unknown key.

The schema is compiled only when the command actually runs, so a broken schema
on a command nobody invokes does not break the rest of the CLI. `npu doctor`
is what checks them all.

`[output].allow_truncated = true` (default `false`) accepts an answer cut
short by `max_tokens` (`finish_reason = "length"`) instead of failing with
exit `4`. `[output].strip_reasoning = true` (default `false`) removes ONE
leading `<think>...</think>` block before the rest of the pipeline runs
(fences, parsing, schema, trim/`max_lines`) — a block anywhere else is left
as content, an unclosed one too. It also DISABLES streaming for the command,
even on a terminal: printing tokens as they arrive would show the reasoning
before it can be stripped.

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
documentation is authoritative. The exact set of keys the binary accepts is
`npu config schema command` (a JSON Schema derived from the parser itself):

- [Writing commands](https://github.com/fmatsos/npu/blob/main/docs/commands.md)
- [Output contracts](https://github.com/fmatsos/npu/blob/main/docs/output.md)
- [`npu config schema`](https://github.com/fmatsos/npu/blob/main/docs/cli.md#npu-config-schema)
- [`npu describe`](https://github.com/fmatsos/npu/blob/main/docs/cli.md#npu-describe)

Related skills: **npu-model**, **npu-config**, **npu-doctor**.

<!-- model/effort: Writing a prompt, choosing an input mode and declaring an output contract need judgement, and a templating mistake is only caught at load time. -->

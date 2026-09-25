# Writing commands

- [File to command name](#file-to-command-name)
- [Anatomy of a command file](#anatomy-of-a-command-file)
- [Frontmatter reference](#frontmatter-reference)
- [Input modes](#input-modes)
- [CLI arguments](#cli-arguments)
- [Prompt templating](#prompt-templating)
- [What is validated, and when](#what-is-validated-and-when)

`npu config schema command` prints the JSON Schema of the frontmatter; see
[`npu config schema`](cli.md#npu-config-schema).

---

## File to command name

The path under `commands/` *is* the command name. There is no registration step.

| File | Command |
| --- | --- |
| `commands/classify.md` | `npu classify` |
| `commands/commit-message.md` | `npu commit-message` |
| `commands/git/review.md` | `npu git review` |
| `commands/ticket/classify.md` | `npu ticket classify` |

Intermediate levels are created automatically, and commands sharing a prefix merge under the same
parent. Running an intermediate level on its own (`npu git`) is a usage error: the CLI parser
prints that level's help **on stderr** and exits with `2`.

```console
$ npu git
Usage: npu git [OPTIONS] [COMMAND]

Commands:
  review  Review a diff

Options:
  -v, --verbose <LEVEL>  Diagnostic verbosity on stderr; stdout always carries the result only [default: warn] [possible values: error, warn, info]
  -h, --help             Print help
```

Seven names are reserved by the built-ins and rejected at load time: `backend`, `config`, `model`,
`doctor`, `describe`, `update` and `help`. The reservation applies to the **first segment only**,
so `commands/git/describe.md` is perfectly valid.

---

## Anatomy of a command file

A command is a Markdown file: TOML frontmatter between `---` fences, then the prompt as the body.
The prompt is the content of the file, not a string squeezed into a config value.

> [!NOTE]
> The fence is `---`, not `+++`. A file still opening with `+++` is rejected at load time with a
> message saying so — never diagnosed as having no frontmatter at all, which would send you
> looking for a line that is right there.

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

---

## Frontmatter reference

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `description` | string | `""` | shown in `npu --help` |
| `model` | string | **required** | must match a model `id` |
| `[input] mode` | string | `"stdin"` | see [Input modes](#input-modes) |
| `[args.<name>]` | table | none | see [CLI arguments](#cli-arguments) |
| `[output]` | table | text, no limit | see [Output contracts](output.md) |
| `[schemas]` | table | none | `<id> = "<name or path>"`, for `{{ schemas.<id> }}` — see [Schemas in the prompt](output.md#schemas-in-the-prompt) |
| `[partials]` | table | none | `<id> = "<name or path>"`, for `{{ partials.<id> }}` — see [Partials](#partials) |
| `system` | string | none | a system-role message sent before the examples and the body — see [System prompt and examples](#system-prompt-and-examples) |
| `[[examples]]` | array of tables | none | fixed few-shot `user`/`assistant` turns — see [System prompt and examples](#system-prompt-and-examples) |
| `[generation]` | table | none | overrides the model's own `[generation]`, key by key — see [Generation parameters](configuration.md#generation-parameters) |

> [!IMPORTANT]
> Unknown keys are **rejected**, not ignored — at the top level, under `[input]`, under
> `[args.*]` and under `[output]`. A typo like `moed = "file"` would otherwise fall back to the
> default silently, so `npu summarize README.md` would read stdin instead of your file without a
> word of warning.

---

## Input modes

```toml
[input]
mode = "stdin_or_file"
```

| Mode | Behaviour |
| --- | --- |
| `stdin` | read standard input to EOF |
| `file` | read the positional `FILE` argument; omitting it is an error |
| `stdin_or_file` | use `FILE` when given, otherwise read stdin |

Modes that accept a file get an optional positional `FILE` argument in their generated CLI.

```sh
cat ticket.md | npu classify      # stdin
npu classify ticket.md            # file
```

---

## CLI arguments

Each `[args.<name>]` table becomes a real flag, in deterministic order.

```toml
[args.language]
short = "l"
required = true
description = "Target language"
```

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `short` | string | none | must be exactly **one** character, not `-`, `h` or `v` |
| `required` | bool | `false` | |
| `description` | string | `""` | shown in the command's `--help` |
| `type` | `string`, `enum`, `integer`, `file` | `string` | validates values before execution |
| `values` | string array | none | required and non-empty only for `enum`; duplicates rejected |
| `min`, `max` | integer | none | only for `integer`; inclusive bounds |

The table key is the long flag: `--language`. Rejected at load time, each naming the file:

- a `short` of more than one character (never silently truncated);
- `short = "-"`, which clap itself rejects;
- two arguments sharing the same `short` letter;
- a name that is empty, contains a space, starts with `-`, or uses characters a placeholder could
  never reference;
- the reserved names `help`, `version`, `FILE` and `verbose`;
- the short letters `-h` (clap's help) and `-v` (the global `--verbose`).

`enum` values are checked by clap, as are `integer` syntax and bounds (usage error, exit 2).
For `file`, the CLI flag names a UTF-8 file whose content replaces `{{ args.<name> }}`;
the file is read after preflight and before stdin. MCP callers pass the content directly.

Values become available to the prompt as `{{ args.<name> }}`.

Default values and repeated or boolean flags are not supported in 0.1.0.

---

## Prompt templating

Templating is deliberately minimal. There are no conditions, no loops and no expressions, and the
only include is a partial inserted verbatim. The goal is configuration that stays deterministic
and statically inspectable.

| Placeholder | Resolves to |
| --- | --- |
| `{{ input }}` | the resolved input (stdin or file) |
| `{{ args.name }}` | the value of a declared argument |
| `{{ env.NAME }}` | an environment variable |
| `{{ schemas.id }}` | a schema declared in `[schemas]`, as JSON |
| `{{ partials.id }}` | a text fragment declared in `[partials]`, verbatim |

Whitespace inside the braces is flexible: `{{input}}`, `{{ input }}` and `{{  input  }}` are the
same. Substitution is never re-applied to substituted content, so an argument value containing
`{{ input }}` is passed through untouched.

An environment variable that is **set but empty** is legitimate and renders as an empty string.
One that is **unset** is an error.

### Partials

A fragment shared by several commands, such as a style guide, a glossary or a paragraph on JSON
discipline, lives in its own file and is declared by each command that uses it:

```toml
[partials]
style = "style-guide"            # bare name: <scope root>/partials/style-guide.md
glossary = "shared/glossary.md"  # a path relative to the scope root, or an absolute one
```

`{{ partials.style }}` then inserts the file's text, in the body, in `system` or in an example.
The rules are those of [`[schemas]`](output.md#schemas-in-the-prompt):

- The scope root is the one the command file was found in. The table belongs to the command, so
  a project that wants its own version of a shared partial overrides the command, not the
  partial.
- An id the table does not declare is rejected at load time.
- The file is read only when the command runs, before its input is read. Missing, unreadable or
  not UTF-8, it is a configuration error (exit `2`) naming the command file and the partial.
  `npu doctor` checks every declared partial in advance.
- A partial is inserted **verbatim** and may not contain a closed placeholder itself: one level,
  no recursion. A partial containing `{{ input }}` is rejected, naming the partial file.

`npu describe` lists a command's partials with their resolved paths, and `--dry-run` shows the
rendered request with the partials inserted.

### Two deliberate constraints

> [!WARNING]
> **A closed placeholder that is not recognised is an error at load time**, never copied through
> verbatim. A misspelled `{{ args.langauge }}` would otherwise reach the model as literal text,
> and the model would answer something plausible. That is the most expensive failure mode
> available here, because it is invisible.
>
> The consequence is that a prompt can no longer contain `{{ foo }}` as literal text. An unclosed
> `{{` is left alone, since nothing can distinguish intent from a typo there.

> [!WARNING]
> **An argument referenced by the prompt must be `required = true`.** The prompt cannot be
> rendered without it, so declaring it optional is a contradiction in the file. It is rejected at
> load rather than silently promoted — honouring configuration differently from how it is
> declared is exactly what this project avoids. Rejecting it at load also lets
> [`npu describe`](cli.md) and [`npu doctor`](cli.md) see the file is broken *without running it*.
>
> This constraint will stop making sense the day default values exist. It reflects the current
> state, not a permanent truth.

---

## System prompt and examples

```toml
---
description = "Classify a support ticket"
model = "qwen-fast"
system = "You are a deterministic classifier. Answer with JSON only."

[[examples]]
user = "ticket: printer on fire"
assistant = '{"category":"hardware","confidence":0.98}'
---
Classify: {{ input }}
```

`system` (optional string) and `[[examples]]` (optional array of `{ user, assistant }` pairs) let
a command steer a model that follows a fixed system instruction and a few fixed demonstrations
more reliably than a single free-text prompt — the most effective non-agentic lever on output
shape for a small local model.

The request sent to the backend becomes, in this order: the `system` message (if declared), each
example's `user`/`assistant` pair (in file order), then the rendered body as the final `user`
message. **A command declaring neither key sends exactly what it always has** — a single `user`
message.

Both `system` and every example field are templated with the same placeholders as the body
(`{{ args.* }}`, `{{ env.* }}`, `{{ schemas.* }}`, `{{ partials.* }}`), with one exception: **`{{ input }}` is
rejected there at load time.** The input is the user's own turn, rendered separately as the last
message — referencing it from `system` or an example would not mean what it looks like it means.

An argument referenced only from `system` or an example is held to the same rule as one
referenced from the body: it must be declared `required = true` ([see above](#two-deliberate-constraints)).
An environment variable they reference is resolved at the same preflight step as the body's own
placeholders — before the input is read.

`system` cannot be blank (empty after trimming), and every example needs both non-empty `user`
and `assistant` fields: a key present but pointless is read and rejected, not silently ignored.

`npu describe` reports the raw `system` template (never resolved — `describe` documents the file,
it does not run it) and the **count** of declared examples, never their content.

---

## What is validated, and when

Everything knowable without the input is checked **before** the input is read.

```text
resolve command → collect arguments → check placeholders resolve
                → read input → render prompt → call backend → apply output contract
```

This ordering matters in a pipeline. Without it, `git diff | npu commit-message` would drain the
whole diff before failing on an unset environment variable — work lost, and a non-replayable
input lost for good.

| Checked at load (exit `2`) | Checked before reading input (exit `2`) | Checked at runtime |
| --- | --- | --- |
| frontmatter syntax, unknown keys | declared arguments are present | backend reachability (exit `3`) |
| `model` exists, resolves to a backend operation | `{{ env.* }}` variables are defined | output contract (exit `4`) |
| argument names, `short` letters | | |
| every placeholder is recognised and declared | | |
| reserved command names, `verbose`/`-v` collisions | | |

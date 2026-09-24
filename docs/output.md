# Output contracts

- [The stdout contract](#the-stdout-contract)
- [Declaring an output contract](#declaring-an-output-contract)
- [Truncated answers](#truncated-answers)
- [Text output](#text-output)
- [JSON output](#json-output)
- [JSON Schema validation](#json-schema-validation)
- [Exit codes](#exit-codes)

---

## The stdout contract

**stdout carries the command result and nothing else.** Diagnostics, warnings and runtime
information go to stderr, always.

```sh
npu summarize README.md > summary.txt    # the file contains the summary, nothing more
cat ticket.md | npu classify | jq .      # safe to pipe into a JSON tool
```

This holds on failure paths too: when a command fails, stdout is empty — zero bytes — and the
error is on stderr. It holds for argument errors raised by the CLI parser itself, and for the
built-ins, whose report *is* their result and therefore goes to stdout.

It also holds whatever `--verbose` says. Verbosity moves a threshold on the **diagnostic** stream
only: `--verbose info` adds engine traces to stderr and changes stdout by not one byte. See
[Verbosity](cli.md#verbosity).

---

## Declaring an output contract

```toml
[output]
format = "json"
schema = "schemas/classification.json"
```

| Key | Type | Default | Notes |
| --- | --- | --- | --- |
| `format` | `"text"` \| `"json"` | `"text"` | |
| `schema` | name or path | none | JSON only; see [Schema paths](#schema-paths) |
| `max_lines` | integer | none | text only |
| `allow_truncated` | boolean | `false` | accept an answer cut short by `max_tokens`; see [Truncated answers](#truncated-answers) |

Rejected at load time, naming the command file:

- `schema` together with `format = "text"` — a schema means nothing for free text;
- `max_lines` together with `format = "json"` — likewise;
- any unknown key under `[output]`.

`format = "json"` **without** a schema is allowed: the response is then only checked for being
well-formed JSON.

### Schema paths

A schema is declared in one of three forms:

| Form | Example | Resolves to |
| --- | --- | --- |
| bare name | `schema = "classification"` | `<scope root>/schemas/classification.json` |
| relative path | `schema = "schemas/classification.json"` | `<scope root>/schemas/classification.json` |
| absolute path | `schema = "/srv/schemas/ticket.json"` | itself |

A value without a `/` and without a `.json` suffix is a name; anything else is a path. A relative
path resolves against the **scope root** of the command file, not against the current directory
and not against the command file itself — `schemas/` is a sibling of `commands/`. A command coming
from `/etc/npu` therefore looks in `/etc/npu/schemas/`. The depth of the command path makes no
difference: `commands/git/review.md` still resolves against the scope root.

### Sending the schema to the model

When the command's backend declares `structured_output = true` (see
[Configuration](configuration.md#structured_output-optional)), the output schema is sent with the
request as an OpenAI `response_format` of type `json_schema`: a server that supports it constrains
the model's answer to the schema, so the prompt does not need to describe the expected shape. The
answer is validated against the schema afterwards either way.

### Schemas in the prompt

A command can also paste schemas into its prompt. Declare them in a `[schemas]` table, one id per
schema, in any of the three forms above, and reference them with `{{ schemas.<id> }}`:

```toml
[schemas]
ticket = "ticket"

[output]
format = "json"
schema = "ticket"
```

```markdown
Answer with a JSON object matching this schema:

{{ schemas.ticket }}
```

A placeholder naming an id missing from `[schemas]` is rejected at load time, naming the command
file. The placeholder renders the schema document as compact JSON.

Schemas are loaded **only when the command actually runs** — before its input is read and
before the backend is contacted. A schema that is
missing or malformed on a command nobody invokes does not break the rest of the CLI. Checking all
of them is what [`npu doctor`](cli.md#npu-doctor) is for.

---

## Text output

The response is trimmed of leading and trailing whitespace. Nothing is parsed, nothing is
unwrapped.

```toml
[output]
format = "text"
max_lines = 1
```

If `max_lines` is declared and the response has more non-empty lines than that, the command fails
with exit code `4`. The output is **never silently truncated** — a response that does not meet
the declared contract is a failure, not something to repair.

---

## Truncated answers

A backend that stops generating because it hit `max_tokens` (its own default, or the model's
declared `[generation].max_tokens`) reports it as `finish_reason = "length"`. `npu` treats that as
an execution failure by default — exit code `4` — exactly like a schema violation or an
`max_lines` overrun: a cut-off answer did not honor the command's contract any less than a
malformed one.

```toml
[output]
allow_truncated = true
```

Setting `allow_truncated = true` accepts the truncated answer as-is instead: it is finalized and
written to stdout like any other answer, and the command exits `0`.

Truncation never triggers the [fallback](configuration.md#fallback-optional) retry: the fallback
exists for a prompt an NPU-served model refuses as too long, not for an answer that ran out of
`max_tokens` — the model answered, it just did not finish.

The behavior differs slightly with the terminal versus a pipe:

- **piped or redirected** (`npu ... > file`, `npu ... | jq .`): stdout stays **completely empty**
  on a truncated answer that is not accepted — the answer never reaches it, byte one included.
- **a terminal, streaming**: tokens already reached the screen as they arrived, before `npu` could
  know the stream would end truncated. The exit code is still `4`; only the closing frame is
  skipped. A calling agent reads the exit code and stderr, never the terminal's screen, so this is
  not a contract violation — only a human-facing display detail.

---

## JSON output

```toml
[output]
format = "json"
schema = "schemas/ticket.json"
```

The pipeline is:

```text
model response → strip Markdown fences → parse JSON → validate against schema → stdout
```

### Fenced responses

Models very often wrap their JSON in a Markdown code fence. `npu` removes an opening fence at the
start and its closing fence at the end, with or without a language tag, tolerating surrounding
whitespace:

````text
```json
{"category": "bug", "confidence": 0.91}
```
````

A fence appearing in the *middle* of the response is left alone — that is content, not wrapping.

### Normalised output

What reaches stdout is the compact serialisation of the parsed value, so stdout is always valid
JSON whatever the model wrapped around it:

```sh
cat ticket.md | npu classify | jq .category
```

---

## JSON Schema validation

Schemas are ordinary JSON Schema documents:

```json
{
  "type": "object",
  "required": ["category", "confidence"],
  "properties": {
    "category": { "type": "string" },
    "confidence": { "type": "number", "minimum": 0, "maximum": 1 }
  },
  "additionalProperties": false
}
```

Validation failures list **every** violation, not just the first one, so a prompt can be fixed in
one pass instead of one error at a time.

A response that is not valid JSON, or that violates the schema, is an execution failure with exit
code `4`. `npu` does not retry, does not reformulate, and does not ask the model again — the
specification is explicit that invalid structured output is a failure. This matters most when the
CLI is driven by another program, which needs a stable contract rather than a best effort.

---

## Exit codes

| Code | Variant | Meaning |
| ---: | --- | --- |
| `0` | — | success |
| `1` | I/O | unreadable file, broken pipe |
| `2` | Configuration | your files are wrong — the message names the file |
| `3` | Backend | unreachable, or a non-2xx HTTP response |
| `4` | Output | the model's answer violated the declared contract |

The distinction between `2` and `4` is the useful one for a calling program: `2` means *your
configuration is broken*, `4` means *your configuration is fine and the model answered badly*.
A missing or malformed schema file is `2`, because the fault is in the configuration, even though
it is only discovered when the command runs.

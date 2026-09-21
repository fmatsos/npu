# Output contracts

- [The stdout contract](#the-stdout-contract)
- [Declaring an output contract](#declaring-an-output-contract)
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
| `schema` | path | none | JSON only; relative to the **scope root** |
| `max_lines` | integer | none | text only |

Rejected at load time, naming the command file:

- `schema` together with `format = "text"` — a schema means nothing for free text;
- `max_lines` together with `format = "json"` — likewise;
- any unknown key under `[output]`.

`format = "json"` **without** a schema is allowed: the response is then only checked for being
well-formed JSON.

### Schema paths

`schema = "schemas/classification.json"` resolves against the **scope root** of the command file,
not against the current directory and not against the command file itself — `schemas/` is a
sibling of `commands/`. A command coming from `/etc/npu` therefore looks in `/etc/npu/schemas/`.
The depth of the command path makes no difference: `commands/git/review.md` still resolves
against the scope root. An absolute path is used as-is.

The schema is loaded and compiled **only when the command actually runs**. A schema that is
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

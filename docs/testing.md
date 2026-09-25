# Testing configured prompts

`npu config test` runs regression cases for configured commands. A case is a TOML file at
`.npu/tests/<command path>/<case>.toml`. For example, `tests/git/review/hardware.toml`
tests `npu git review`. Cases from system, user and project scopes are combined; a more local
case replaces one with the same command path and case name. A selection with no cases is an
error (exit `2`). `npu config schema test` prints the JSON Schema of a case file; see
[`npu config schema`](cli.md#npu-config-schema).

```toml
args = { kind = "ticket" }
input = { file = "hardware.txt" } # a file next to this case; or input = "inline text"

[expect]
exit_code = 0
"/category" = "hardware"
"/confidence" = { min = 0.8 }
```

`args` names arguments declared by the command; required arguments must be supplied.
`input` is required. A file input is read as UTF-8 with the ordinary input size limit and
must name a file next to the case. Every case and expectation key is checked before any request.

For JSON commands, keys beginning with `/` are JSON pointers. Their values are exact JSON
values, or a numeric minimum as `{ min = 0.8 }`. A missing pointer fails the case. For text
commands, use `lines = 1` to check the number of nonempty lines and
`contains = ["feat:"]` to require literal substrings. Only `exit_code = 0` (the default)
and `exit_code = 4` (output contract rejection) are valid expectations.
An `exit_code = 4` case cannot also declare output comparisons because it produces no
validated output.

```sh
npu config test                      # all cases, sequentially
npu config test git review           # one command path
npu config test --model gpu-twin     # run the same cases with another model
npu config test --repeat 3 --json    # measure distinct final outputs
npu config test --dry-run --json     # list cases and rendered messages, no request
```

The normal report has `COMMAND CASE RESULT DURATION` columns. `--json` returns an array
whose rows contain `command`, `case`, `result`, `duration_ms`, `runs`, `distinct_outputs`
and, on failure, `message`. Each case runs `N` times with `--repeat N`; distinct outputs
are counted after the command's output contract, and are zero if no run produced a valid
output. The runner finishes the suite when expectations fail (exit `4`). Malformed cases
(exit `2`) and backend errors (exit `3`) stop the suite with empty stdout. A dry run prints
the rendered messages for each case and never contacts the backend.
With `NPU_STATS_FILE` set, every run of a case also appends a
[statistics record](cli.md#execution-statistics) carrying the case name.

# Changelog

Every notable change to `npu`, newest first. Versions follow
[semantic versioning](https://semver.org); pre-1.0, a breaking change bumps
the minor.

## [0.8.0] - 2026-09-25

### Added

- `npu config schema backend|model|command|test` prints the JSON Schema of each configuration
  format, derived from the parser itself; point an editor at it (Taplo's `#:schema`) for
  completion and unknown-key warnings ([`3b4b900`](https://github.com/fmatsos/npu/commit/3b4b900ee7cfe35f281e4abd37c89e03ee5dd359))
- `[partials]` and `{{ partials.<id> }}`: a shared text fragment (style guide, glossary) lives
  in `<scope root>/partials/` and is inserted verbatim in the body, `system` or an example; a
  missing partial exits `2` naming both files, before the input is read
  ([`9a7fcf7`](https://github.com/fmatsos/npu/commit/9a7fcf7d52d4fa226c6eeb8bf56c6a9115445d91))
- `[output].extract = "/pointer"` on a JSON command writes one value instead of the document — a
  string bare, anything else as compact JSON — after the whole document passed its schema; a
  pointer the answer lacks exits `4`. CLI stdout only: MCP and `config test` keep the document
  ([`1bd5cf9`](https://github.com/fmatsos/npu/commit/1bd5cf905e46e1542b97fa89abedee83ae165be3))
- `NPU_STATS_FILE`: every run of a configured command (CLI, `config test` case, MCP tool call)
  appends one JSON line with the requested and answering model, fallback, backend, duration,
  token usage, finish reason and exit code — never the prompt, the answer or a header value. A
  failed write is a warning and changes nothing else
  ([`a225360`](https://github.com/fmatsos/npu/commit/a225360faefda38ea2cb9a5684074d10dc1c3c75), [`9ca7dda`](https://github.com/fmatsos/npu/commit/9ca7dda016cae0b955442d4bb72e6e74dccfb6df))
- `max_concurrent = 1` on a backend serializes its requests across every `npu` process on the
  machine (an advisory lock in the state directory): a second invocation waits instead of
  failing with a `5xx` on a single NPU. `--no-wait` makes a busy backend a backend failure
  instead, which the model's `fallback` absorbs or which exits `3`
  ([`6831f83`](https://github.com/fmatsos/npu/commit/6831f837ed9e11e3e8da9a55c1fde5cd5647528d), [`b412aa0`](https://github.com/fmatsos/npu/commit/b412aa0b98e146b87c02b4b6e6a0e588ff9159e2))
- `protocol = "embeddings"` on an `[operations.<name>]` table: the rendered prompt is sent as
  `input` and the answer is the vector, as the command's JSON output. `chat` stays the default,
  so existing backends are unchanged ([`aedef68`](https://github.com/fmatsos/npu/commit/aedef6874a4b19a304edfafd46ee9e1eddcd60b5))
- `[input] mode = "binary"` and `protocol = "transcriptions"`: a command uploads a file (or stdin)
  as bytes in a multipart request, the prompt as an optional hint, and prints the transcript
  (`npu transcribe memo.wav`). Binary commands are not offered as MCP tools
  ([`96b3caf`](https://github.com/fmatsos/npu/commit/96b3caf17122ba53bca507e9bd77229597be35f9), [`b412aa0`](https://github.com/fmatsos/npu/commit/b412aa0b98e146b87c02b4b6e6a0e588ff9159e2))

### Changed

- **Breaking**: `no-wait` is now a reserved argument name, taken by the new `--no-wait` flag;
  rename any `[args."no-wait"]` a command declares
  ([`6831f83`](https://github.com/fmatsos/npu/commit/6831f837ed9e11e3e8da9a55c1fde5cd5647528d))
- A command running an embeddings or transcriptions model is checked against that protocol when
  it runs and by `npu doctor` (chat-only keys such as `system` are rejected, exit `2`); an
  embeddings model rejects `fallback` and `[generation]`, and a fallback must speak its model's
  protocol, at load time ([`aedef68`](https://github.com/fmatsos/npu/commit/aedef6874a4b19a304edfafd46ee9e1eddcd60b5), [`96b3caf`](https://github.com/fmatsos/npu/commit/96b3caf17122ba53bca507e9bd77229597be35f9))

### Fixed

- `npu mcp serve` no longer sends a non-object JSON answer as `structuredContent`, nor advertises
  an output schema whose root is not an object ([`f466b41`](https://github.com/fmatsos/npu/commit/f466b4191201d2e4ebbf5d7218e00d7ed15f7b8d))

**Full changelog**: [`v0.7.1...v0.8.0`](https://github.com/fmatsos/npu/compare/v0.7.1...v0.8.0)

## [0.7.1] - 2026-09-25

### Fixed

- The MCP pipeline error-envelope integration test now reaches the configured-command pipeline
  instead of passing an undeclared CLI argument and testing clap's usage envelope
  ([`d3856e6`](https://github.com/fmatsos/npu/commit/d3856e6ca8b1873751f1a0e33c236e0a7fa7771d))

**Full changelog**: [`v0.7.0...v0.7.1`](https://github.com/fmatsos/npu/compare/v0.7.0...v0.7.1)

## [0.7.0] - 2026-09-25

### Added

- `npu mcp serve` exposes configured business commands as MCP tools over stdio using `rmcp`, with
  MCP `2026-07-28` discovery, typed argument schemas, output schemas, and serialized blocking
  execution ([`dee94ce`](https://github.com/fmatsos/npu/commit/dee94ce8e5342336947e6ff4c3b1d9799b0ff8b5))
- Typed command arguments support enums, bounded integers, and UTF-8 file content substitution;
  MCP calls use the literal `"mcp.input"` property for command input
  ([`dee94ce`](https://github.com/fmatsos/npu/commit/dee94ce8e5342336947e6ff4c3b1d9799b0ff8b5))
- Pipeline failures can be rendered as structured JSON envelopes with their error family and
  stable exit code ([`dee94ce`](https://github.com/fmatsos/npu/commit/dee94ce8e5342336947e6ff4c3b1d9799b0ff8b5))

### Changed

- **Breaking**: `mcp` is now a reserved top-level command group; rename any configured command
  whose first path segment is `mcp` ([`dee94ce`](https://github.com/fmatsos/npu/commit/dee94ce8e5342336947e6ff4c3b1d9799b0ff8b5))

**Full changelog**: [`v0.6.1...v0.7.0`](https://github.com/fmatsos/npu/compare/v0.6.1...v0.7.0)

## [0.6.1] - 2026-09-24

### Changed

- Linux x86-64 ships as one binary, `x86_64-unknown-linux-gnu`, for every glibc-based
  distribution; the separate Fedora and Arch builds are gone (they were the same program).
  `npu update` no longer reads `/etc/os-release`. A Fedora or Arch install on 0.6.0 or earlier
  updates to this release as usual; one that skips it must reinstall from the releases page
  once, as later manifests no longer list `x86_64-fedora` or `x86_64-arch`
  ([`8590376`](https://github.com/fmatsos/npu/commit/85903763f80dabf261962f9161536d932bb172b1))

**Full changelog**: [`v0.6.0...v0.6.1`](https://github.com/fmatsos/npu/compare/v0.6.0...v0.6.1)

## [0.6.0] - 2026-09-24

### Added

- `--dry-run` on every configured command prints the request `npu` would send — URL, header
  names (values redacted) and body — as JSON on stdout, without sending it or starting a runtime.
  The body is built by the same code as a real call, including `"stream": true` when the real
  call would stream ([`3a21b65`](https://github.com/fmatsos/npu/commit/3a21b65d83dc9a9553504fdf25161d9c03b29177), [`1933062`](https://github.com/fmatsos/npu/commit/193306261cefee1239f54bb76e0792313a33e30d))
- `--model <ID>` on every configured command uses another configured model for that one call; an
  unknown id exits `2` naming the available ones, before the input is read ([`5c0f366`](https://github.com/fmatsos/npu/commit/5c0f3660fbdbaa3e6406ccaf1f91c3cdc7d32af7))
- `--json` on `doctor` / `config check`, `backend status` and `config models`: the same report as
  a JSON array (`kind` is `"config"` or `"reachability"`) for a calling program ([`5a266c2`](https://github.com/fmatsos/npu/commit/5a266c26310065c9cbf6278021d21aaa72569fe8))
- `npu describe` with no argument lists every describable path, built-in groups included
  ([`5a266c2`](https://github.com/fmatsos/npu/commit/5a266c26310065c9cbf6278021d21aaa72569fe8), [`566e600`](https://github.com/fmatsos/npu/commit/566e60095ff9b65d5f98a615bd71965f0e8f0e65))
- `--error-format json` puts a command-line usage error on stderr as one JSON line
  (`{"kind":"usage","message":...}`), including a missing subcommand and `npu help <unknown>`;
  `--help` and `--version` are never affected ([`5a266c2`](https://github.com/fmatsos/npu/commit/5a266c26310065c9cbf6278021d21aaa72569fe8), [`04cc703`](https://github.com/fmatsos/npu/commit/04cc70333f25e765462ec8e56da55a0754ae0786))
- The project scope is found by walking up from the current directory to the nearest `.npu`,
  stopping at the repository root (`.git`) or at `$HOME`; `--config-dir <DIR>` or
  `NPU_CONFIG_DIR` names it directly, and `npu doctor` reports which one was used
  ([`6f09ed9`](https://github.com/fmatsos/npu/commit/6f09ed93cf82cd5491fcb4b739cfeff98db02466), [`dea2e5f`](https://github.com/fmatsos/npu/commit/dea2e5f0a0d0aad12873128cf3e39acec2a793d8), [`d17661f`](https://github.com/fmatsos/npu/commit/d17661fbedb59ce6dfedaf274e4a7c7334a88df1))
- Backends accept a `[headers]` table sent with every request, for hosted or authenticated
  OpenAI-compatible servers. Values may only use `{{ env.NAME }}`, are resolved before the input
  is read, and are never logged ([`4226aa3`](https://github.com/fmatsos/npu/commit/4226aa35f4d3ed5268a79fe07303cb5b9ab3659c), [`92498e4`](https://github.com/fmatsos/npu/commit/92498e4b62f5d5428b3bd0bf41a59ad5dc74abe5))
- Commands accept a `system` prompt and `[[examples]]` (few-shot user/assistant turns). A command
  declaring neither sends exactly the same request as before ([`2c9c27e`](https://github.com/fmatsos/npu/commit/2c9c27e323091d7e73486b1809eb4a2ad11214f0))
- Model `[generation]` gains `seed`, `top_p`, `stop` and a free-form `[generation.extra]`
  forwarded as-is (e.g. `chat_template_kwargs`); a command may override any of them for itself,
  key by key ([`073e541`](https://github.com/fmatsos/npu/commit/073e541ad9deaba6312b1bb1a72a59ebc3d28984), [`92498e4`](https://github.com/fmatsos/npu/commit/92498e4b62f5d5428b3bd0bf41a59ad5dc74abe5))
- `[output] strip_reasoning = true` removes a leading `<think>…</think>` block before the output
  contract is applied ([`faab3f1`](https://github.com/fmatsos/npu/commit/faab3f1f4431f111f39e9b093cf810a47f8f17bd))
- The token usage reported by the server is logged at `--verbose info` ([`7d4e719`](https://github.com/fmatsos/npu/commit/7d4e7196c8ad7bd23d88688ed531cc61f3927dba))
- Shell completions: `COMPLETE=bash npu` (or `zsh`, `fish`…) prints the registration script;
  completion covers built-ins, configured commands and their arguments
  ([`042f9f1`](https://github.com/fmatsos/npu/commit/042f9f12fde99d2045b79e333e547d917224fbc5), [`ef5230f`](https://github.com/fmatsos/npu/commit/ef5230f9f11ae0a5ebba4896f60f64fa624e807b), [`155ca00`](https://github.com/fmatsos/npu/commit/155ca005571248735e3bcd0ece0f17464e2742a7))
- `backend tune --dry-run` also prints the NPU calibration constants its estimate uses
  ([`a178c83`](https://github.com/fmatsos/npu/commit/a178c832de7e0073f3c160a499d73eae56301813), [`e22dbac`](https://github.com/fmatsos/npu/commit/e22dbac9fd7c6cb5ab5b91b9fd29e74e1e5914c3))
- A Cargo feature, `hardware-tooling` (on by default), holds `model discover` and `backend tune`;
  building with `--no-default-features` leaves them out ([`06ed679`](https://github.com/fmatsos/npu/commit/06ed679bcf407cced220486607418ea3bd8b3fb7), [`55f413d`](https://github.com/fmatsos/npu/commit/55f413d60f418d42308c5bc7c64d9da153bf88af), [`d84d13b`](https://github.com/fmatsos/npu/commit/d84d13b202aeca6d8f599244985ba496e649e546))

### Changed

- **Breaking**: an answer cut at `max_tokens` (`finish_reason = "length"`) now exits `4`, naming
  the model that answered and the limit in effect, instead of exiting `0` with a truncated
  answer. Set `allow_truncated = true` under the command's `[output]` to keep the old behaviour.
  A truncation never triggers the fallback ([`7d4e719`](https://github.com/fmatsos/npu/commit/7d4e7196c8ad7bd23d88688ed531cc61f3927dba), [`142600a`](https://github.com/fmatsos/npu/commit/142600a2f10dbce4dfd8309290a3a02c3e7e56c8))
- **Breaking**: `dry-run`, `model`, `error-format` and `config-dir` are now reserved argument
  names. A command file declaring one of them under `[args]` is rejected at load time, naming
  the file; rename the argument ([`3a21b65`](https://github.com/fmatsos/npu/commit/3a21b65d83dc9a9553504fdf25161d9c03b29177), [`5c0f366`](https://github.com/fmatsos/npu/commit/5c0f3660fbdbaa3e6406ccaf1f91c3cdc7d32af7), [`5a266c2`](https://github.com/fmatsos/npu/commit/5a266c26310065c9cbf6278021d21aaa72569fe8), [`6f09ed9`](https://github.com/fmatsos/npu/commit/6f09ed93cf82cd5491fcb4b739cfeff98db02466))
- **Breaking**: a `.npu` in a parent directory is now loaded when `npu` runs from a
  subdirectory. A parent `.npu` you did not mean to use must be moved, or pass `--config-dir`
  ([`6f09ed9`](https://github.com/fmatsos/npu/commit/6f09ed93cf82cd5491fcb4b739cfeff98db02466))
- **Breaking**: on Windows, the system and user scopes are `%ProgramData%\npu` and
  `%APPDATA%\npu` (else `%USERPROFILE%\.config\npu`). A configuration placed under
  `HOME` or `XDG_CONFIG_HOME` as a workaround must move there ([`5cf5371`](https://github.com/fmatsos/npu/commit/5cf53713ae1e3f6705d7c26c9d30387dd02a43e3))
- A stream that reports an error, or ends with no content, now fails with exit `3` instead of
  returning a partial or empty answer ([`7d4e719`](https://github.com/fmatsos/npu/commit/7d4e7196c8ad7bd23d88688ed531cc61f3927dba))
- The OpenVINO architecture registry used by `model discover` is pinned to optimum-intel
  `v2.2.0` instead of its moving `main` branch ([`5a4f1c2`](https://github.com/fmatsos/npu/commit/5a4f1c230a649664726fb924f89a046d116b9707))

### Fixed

- `npu update` no longer warns that a valid configuration is invalid after every successful
  update ([`64934dc`](https://github.com/fmatsos/npu/commit/64934dccd337fb8cf1881a5f499c762754c06fde))
- An unreadable, missing, non-UTF-8 or oversized input (over 64 MiB) is reported naming the
  file or `stdin`; still exit `1` ([`8b3f187`](https://github.com/fmatsos/npu/commit/8b3f187dbfe3befabde1f45404103e4b8ecfd2df), [`cfa24e4`](https://github.com/fmatsos/npu/commit/cfa24e45a25b6bfb957e19b7c924a9f7e276f0e1))
- `model discover` now recognises architectures registered only through optimum-intel's shared
  text-generation task list ([`5a4f1c2`](https://github.com/fmatsos/npu/commit/5a4f1c230a649664726fb924f89a046d116b9707))
- Usage lines name the binary `npu` on Windows too, instead of `npu.exe` ([`b617677`](https://github.com/fmatsos/npu/commit/b617677d2e73fb5ff780c0ea5564ea0ea884e2ef))
- A backend error without a fallback keeps its URL and HTTP status, and a failing fallback is
  reported under its own backend ([`142600a`](https://github.com/fmatsos/npu/commit/142600a2f10dbce4dfd8309290a3a02c3e7e56c8))

**Full changelog**: [`v0.5.1...v0.6.0`](https://github.com/fmatsos/npu/compare/v0.5.1...v0.6.0)

## [0.5.1] - 2026-09-23

### Added

- A free-text answer (`format = "text"`, no `max_lines`) is streamed to a terminal: each token
  is printed as the model produces it, so the answer starts showing within a second instead of
  at the end of the generation. The fallback still takes over while nothing is on screen (a
  stopped container, a prompt the NPU refuses). Once a token is shown, a failure exits `3`
  after the partial answer. A JSON or `max_lines` answer, and anything written to a pipe or a
  file, still arrives in one piece, byte for byte as before
  ([`ecce23c`](https://github.com/fmatsos/npu/commit/ecce23cd3e8a4c965b2a09384227fdfcd594441d))

**Full changelog**: [`v0.5.0...v0.5.1`](https://github.com/fmatsos/npu/compare/v0.5.0...v0.5.1)

## [0.5.0] - 2026-09-23

### Added

- `npu model discover [words]` searches Hugging Face for the models this host can run, on CPU,
  GPU or NPU. When the `llmfit` CLI is on `PATH`, a model is kept on its verdict (`Perfect` or
  `Good`, score at least `--min-score`). Otherwise, its INT4 weights must fit `--max-memory`
  percent of the RAM.
  - `--backend openvino|llamacpp|mlx` narrows the list to one engine's packaging. It also
    accepts a configured backend's identifier, whose engine is read from its runtime. `openvino`
    keeps the architectures `optimum-intel` exports, read from its registry at run time, with
    original, open weights.
  - `--npu` adds the Intel NPU check, and exits `3` on a host without one.
  - A third-party GGUF inherits llmfit's verdict for the model it packages.
  - `--sort column[:asc|desc],...` orders the report by one or more columns.
  - On a terminal the report is coloured; a pipe gets a plain table.
  - It needs no configuration, except to resolve a backend identifier.
  ([`496d534`](https://github.com/fmatsos/npu/commit/496d53415e46a7056ccabdac1516d435166ab6d7)) ([`fa8ad71`](https://github.com/fmatsos/npu/commit/fa8ad719a1b1c30f60fb94fb442be71c39c65461)) ([`86bf561`](https://github.com/fmatsos/npu/commit/86bf5618715ddf8cc3e6032867cb81047249741c)) ([`aebbbcb`](https://github.com/fmatsos/npu/commit/aebbbcbf02c24f801d488ac5a4a9211ce0643f35)) ([`00974f2`](https://github.com/fmatsos/npu/commit/00974f21d786150cabcb4236221451ee00ca5b36)) ([`634054c`](https://github.com/fmatsos/npu/commit/634054c81316982625c21376ce088cda822111d9))
- `npu backend tune` sizes the context of every exported model from the model's `config.json`
  and the host's RAM, and writes it to `graph.pbtxt`. It also sets each model file's
  `[generation].max_tokens` to the answer length. Two limits apply: `--max-memory` (percent of
  the RAM, default 50) and `--max-models` (how many run at once, default all).
  - On NPU, it writes `MAX_PROMPT_LEN` and `MIN_RESPONSE_LEN`, and enables
    `NPUW_LLM_ENABLE_PREFIX_CACHING`.
  - On GPU, it bounds `cache_size` and sets `max_num_seqs` to 4. `--kv-u8` stores the KV cache
    as u8, which holds about twice the context.
  - `--npu` or `--gpu` tunes one device, so each can get its own limits. `--dry-run` prints the
    plan without writing.
  - It replaces the `npu-context.py` script of the `npu-export` skill.
  ([`25504d3`](https://github.com/fmatsos/npu/commit/25504d382d58e9b7871b4ce2a9e9ba575b5e9b74)) ([`c3208b0`](https://github.com/fmatsos/npu/commit/c3208b043ec9870f73e43b666cd15e28e8606f7f)) ([`974653c`](https://github.com/fmatsos/npu/commit/974653c849c6e2c0272c2b9fd7072c35a1a03a11))
- A backend declaring `structured_output = true` receives the command's output schema as an
  OpenAI `response_format` (`json_schema`). The model's decoding is constrained by it, and the
  answer is still validated afterwards ([`9d4eae4`](https://github.com/fmatsos/npu/commit/9d4eae48dc342855f7d123f0ca8db1236fa39b40))
- Commands can declare a `[schemas]` table and paste a schema into the prompt with
  `{{ schemas.<id> }}`. An undeclared id is rejected at load time, naming the file, and
  `npu doctor` checks each entry ([`9d4eae4`](https://github.com/fmatsos/npu/commit/9d4eae48dc342855f7d123f0ca8db1236fa39b40))
- On a terminal, a command's answer is set apart from the command line: a blank line, and a
  header naming the model that actually answered (the fallback, when it took over). A pipe or a
  file still receives the answer alone, byte for byte ([`1886c98`](https://github.com/fmatsos/npu/commit/1886c980902d196d380681d526b532293f9e621e)) ([`9b5fc96`](https://github.com/fmatsos/npu/commit/9b5fc968821f669c4b58f4bb27e6f05ebdb91799))

### Changed

- **Breaking**: `model` is now a reserved name, for the new `npu model` group. Rename a
  configured command whose file is `model.md` or sits under `model/`
  ([`496d534`](https://github.com/fmatsos/npu/commit/496d53415e46a7056ccabdac1516d435166ab6d7))
- **Breaking**: a `schema` value that is a bare name, with no `/` and no `.json`, now resolves
  to `<scope root>/schemas/<name>.json`. A path (relative to the scope root) or an absolute path
  resolves as before. A bare file name at the scope root must be written with its `.json`
  extension ([`9d4eae4`](https://github.com/fmatsos/npu/commit/9d4eae48dc342855f7d123f0ca8db1236fa39b40))
- A fallback taking over is logged at `info` instead of `warn`: it is the designed path, and the
  command still succeeds. Use `-v info` to see it ([`45418b6`](https://github.com/fmatsos/npu/commit/45418b6034ed4a167bbf756cc947154b283fe185))

### Fixed

- A probe on a stopped container no longer prints Docker's `No such container` on the terminal:
  Docker's stderr is captured and added to the error, shown only when the error is reported
  ([`45418b6`](https://github.com/fmatsos/npu/commit/45418b6034ed4a167bbf756cc947154b283fe185))

**Full changelog**: [`v0.4.0...v0.5.0`](https://github.com/fmatsos/npu/compare/v0.4.0...v0.5.0)

## [0.4.0] - 2026-09-23

### Added

- `npu describe` describes built-ins as well as configured commands, and takes the path as words
  (`npu describe backend serve`, `npu describe git review`; `git/review` still works). Every
  description carries `kind` (`builtin` or `command`); a built-in lists its arguments,
  subcommands and whether it runs with a broken configuration — which `describe` itself does for
  built-ins; a configured command adds its resolved `backend`, its `fallback` and its `source`
  (the file that won and its scope). No existing field changes
  ([`770867a`](https://github.com/fmatsos/npu/commit/770867add1cc492e8c350d2ad0e9f803073ce9c5))
- On a terminal, a spinner while a model is waited on (relabelled when the fallback takes over)
  and while a process backend starts, and a progress bar while `npu update` downloads. Drawn on
  stderr only, only when stderr is a terminal and `--verbose` is above `error`
  ([`fd18b97`](https://github.com/fmatsos/npu/commit/fd18b97924d086fd4d5b0948192cced9dd836d22))
- Colours on a terminal: help and usage errors, the `warn`/`error`/`info` labels, the final error
  line and `doctor`'s check marks. Through a pipe, or with `NO_COLOR`, output is byte for byte what
  it was without them
  ([`c2c04e9`](https://github.com/fmatsos/npu/commit/c2c04e91104e57cbd1897a07665b8c7ce2aec39d))

### Changed

- **Breaking**: the built-ins are grouped. Update scripts as follows:

  | Before | Now |
  | --- | --- |
  | `npu serve`, `stop`, `status`, `logs` | `npu backend serve`, `stop`, `status`, `logs` |
  | `npu models` | `npu config models` |
  | `npu version` | `npu --version`, which prints `npu X.Y.Z` |

  `npu doctor` is unchanged and also available as `npu config check`. The old names are no longer
  recognised (exit `2`, nothing on stdout). The reserved command names shrink to `backend`,
  `config`, `doctor`, `describe`, `update` and `help`: a command file may now be named `serve`,
  `stop`, `status`, `logs`, `models` or `version`
  ([`83130e3`](https://github.com/fmatsos/npu/commit/83130e36f3deb907ec865378cdf218ecb4f713aa))
- `npu --help` lists the configured commands and the built-ins in two separate sections,
  `Commands:` and `Built-ins:`; `npu help <command>` is listed among the built-ins
  ([`c556e6e`](https://github.com/fmatsos/npu/commit/c556e6e2b066030d2b3451890aafa71dcdb46684)) ([`c2c04e9`](https://github.com/fmatsos/npu/commit/c2c04e91104e57cbd1897a07665b8c7ce2aec39d))

**Full changelog**: [`v0.3.1...v0.4.0`](https://github.com/fmatsos/npu/compare/v0.3.1...v0.4.0)

## [0.3.1] - 2026-09-23

### Fixed

- `npu update` and `npu version` no longer warn about an invalid configuration: neither reads it.
  After an update, the newly installed binary checks the configuration instead of the old one, so
  a key introduced by the new release no longer looks invalid; if the new version does reject it,
  a warning on stderr points to this changelog and the documentation, and the update still exits
  `0`
  ([`3e2fb18`](https://github.com/fmatsos/npu/commit/3e2fb187293dfac95e7d0414ab7b3b4ec3e80966))

**Full changelog**: [`v0.3.0...v0.3.1`](https://github.com/fmatsos/npu/compare/v0.3.0...v0.3.1)

## [0.3.0] - 2026-09-22

### Added

- A backend can be started as a **local process** instead of a container: `[runtime]` with
  `type = "process"`, a `command`, its `arguments`, an optional `[runtime.env]` overlay and a
  `startup_timeout_secs` readiness budget. `npu serve` spawns it, prints its pid only once its port
  answers, and `stop`, `status` and `logs` find it again through a small state record. This is
  what lets a Mac run `llama-server` on Metal, which a Linux container cannot reach. Unix only:
  on Windows such a backend is rejected at load time, naming the file
  ([`552881f`](https://github.com/fmatsos/npu/commit/552881fbd0d0c7483cd986c5e4db46b95d37bbd4),
  [`92d3cc5`](https://github.com/fmatsos/npu/commit/92d3cc50f1a2a06933ec7735802eef34444e1595))
- `npu stop` never signals a process it cannot prove it started: the record keeps the pid and the
  moment the system says it was born, and a recycled pid is forgotten, not killed. Two projects
  that both declare a backend `llamacpp` get separate records and logs, and neither can stop the
  other's server
  ([`552881f`](https://github.com/fmatsos/npu/commit/552881fbd0d0c7483cd986c5e4db46b95d37bbd4),
  [`92d3cc5`](https://github.com/fmatsos/npu/commit/92d3cc50f1a2a06933ec7735802eef34444e1595))
- `fallback` on a model retries once on another model when the first one fails with a backend
  error, which moves a prompt too long for an NPU-compiled graph onto a GPU-served twin. The
  primary failure is always logged at `warn`, so a backend down all day does not pass for a
  working fallback. A fallback naming an unknown model, or itself, is rejected at load time
  ([`bdd564c`](https://github.com/fmatsos/npu/commit/bdd564c9a0dd1e29990daa0ad1089060505c5468))
- `port` on a backend is declared once and read as `{{ backend.port }}` in `base_url` and the
  `[runtime]` lists, so the two can no longer drift apart. `port = "auto"` lets Docker pick a free
  port and reads it back. `npu serve` now reports a backend that is already served, or a fixed
  port held by something else, before starting anything (exit `3`)
  ([`bdd564c`](https://github.com/fmatsos/npu/commit/bdd564c9a0dd1e29990daa0ad1089060505c5468))
- Two Claude Code skills for the model side of an Intel NPU deployment: `npu-discover` finds
  Hugging Face models the NPU can actually run, `npu-export` exports one with `optimum-cli`,
  checks it on CPU and writes the model files, GPU twin included
  ([`8db677f`](https://github.com/fmatsos/npu/commit/8db677fbc1dea9d6019075b345f11e43e7894038),
  [`bdd564c`](https://github.com/fmatsos/npu/commit/bdd564c9a0dd1e29990daa0ad1089060505c5468))
- Two deployment guides: [Intel NPU](https://github.com/fmatsos/npu/blob/main/docs/intel-npu.md) (export, quantization, OVMS, the GPU
  twin) and [Apple Silicon](https://github.com/fmatsos/npu/blob/main/docs/apple-silicon.md) (`llama-server` on Metal, started by
  `npu serve`)
  ([`b508085`](https://github.com/fmatsos/npu/commit/b5080852760ccb126e80f690950d2b9883148c40),
  [`bdd564c`](https://github.com/fmatsos/npu/commit/bdd564c9a0dd1e29990daa0ad1089060505c5468),
  [`f61c33d`](https://github.com/fmatsos/npu/commit/f61c33d3a61d6f78c7141370c2e2f24a6fec3993))

### Changed

- **Breaking**: `npu status` prints `BACKEND  RUNTIME  INSTANCE  URL  STATE` instead of
  `BACKEND  CONTAINER  STATE`. A program reading its columns by position must be updated: the
  instance (container name or pid) is now column 3
  ([`bdd564c`](https://github.com/fmatsos/npu/commit/bdd564c9a0dd1e29990daa0ad1089060505c5468),
  [`ad5803e`](https://github.com/fmatsos/npu/commit/ad5803e6d9f30448c02f095743361a4ee18680ba))
- **Breaking**: `npu models` gains a trailing `FALLBACK` column (`-` when the model declares
  none)
  ([`bdd564c`](https://github.com/fmatsos/npu/commit/bdd564c9a0dd1e29990daa0ad1089060505c5468))
- A backend's runtime is declared in a `[runtime]` table tagged by `type` (`"docker"` or
  `"process"`), so an unsupported family is rejected by name. The `[docker]` table of earlier
  versions is still accepted and means `type = "docker"`; declaring both is rejected, naming the
  file
  ([`ad5803e`](https://github.com/fmatsos/npu/commit/ad5803e6d9f30448c02f095743361a4ee18680ba))
- Release binaries are built with thin LTO: the build takes half the time, and the binary is
  about 1.5 MB larger
  ([`2d3279e`](https://github.com/fmatsos/npu/commit/2d3279e1fc00dc230e5e887dccd4c063c550bc20))

**Full changelog**: [`v0.2.0...v0.3.0`](https://github.com/fmatsos/npu/compare/v0.2.0...v0.3.0)

## [0.2.0] - 2026-09-22

### Added

- `npu version` and `npu update` check, download, verify and install the latest GitHub release
  binary. Both run in degraded mode like `doctor`, since neither depends on the AI configuration
  ([`8003b25`](https://github.com/fmatsos/npu/commit/8003b257edbbb10e330f9fc71d11fa236e7b2252))

### Changed

- **Breaking**: a backend's `[timeouts]` table (`request_secs`, in seconds) is now honoured
  instead of being rejected as an unknown key, and the default request timeout rises from 30s to
  120s to cover a full `max_tokens` generation on a slow accelerator. `request_secs = 0` is
  rejected at load time, naming the file
  ([`e40cd5e`](https://github.com/fmatsos/npu/commit/e40cd5e6a90e9daf801f88034a22610be632137e))

### Fixed

- The `ovms` backend fixture pins the OpenVINO Model Server image to `2026.4.0` (was the floating
  `:latest` tag) and mounts a persistent `--cache_dir`, so NPU/GPU graph compilation happens once
  per model instead of on every container restart
  ([`829e8da`](https://github.com/fmatsos/npu/commit/829e8da063afafb3672a5c3f89b71fada556c091),
  [`c5b4a31`](https://github.com/fmatsos/npu/commit/c5b4a31c056bde9309a4a0023c1e84d62740f9c8))

**Full changelog**: [`v0.1.0...v0.2.0`](https://github.com/fmatsos/npu/compare/v0.1.0...v0.2.0)

## [0.1.0] - 2026-09-21

First release. `npu` runs prompts from the command line, and its command
tree, models, backends and output contracts are **configuration** — adding a
command does not require a rebuild.

### Added

- Commands are Markdown files under `.npu/commands/`, TOML frontmatter fenced
  by `---`; the file's path becomes the command name, so the CLI tree is built
  at startup from a directory scan
  ([`b36ea22`](https://github.com/fmatsos/npu/commit/b36ea220357f0633e1a6f1b269c807272d8e7d2f),
  [`e0f2c5a`](https://github.com/fmatsos/npu/commit/e0f2c5acffebb5c6e659d2cb6b2c7f3bdaca822b))
- Configuration is read from three scopes — `/etc/npu`,
  `$XDG_CONFIG_HOME/npu`, then `./.npu` — where a narrower scope replaces an
  entry of the same id rather than merging into it
  ([`09bc7e1`](https://github.com/fmatsos/npu/commit/09bc7e1a24746a72bf72ad6b7f287c494387c3d1))
- A command declares its own flags in `[args.*]`, and its prompt interpolates
  `{{ input }}`, `{{ args.x }}` and `{{ env.X }}`
  ([`3583dd6`](https://github.com/fmatsos/npu/commit/3583dd6e3bc096624c26d9feebf706b9ab810e58),
  [`98d8774`](https://github.com/fmatsos/npu/commit/98d87746cbbc2a183477cc98cc8e02ae208b709e))
- A command can declare an `[output]` contract with a JSON Schema; an answer
  that violates it exits `4` instead of reaching the caller
  ([`f5abc37`](https://github.com/fmatsos/npu/commit/f5abc37a0b656b3792ba5e72882f3a9e4c3e834f))
- `npu doctor` diagnoses a setup, `npu models` lists the declared aliases and
  `npu describe` prints what a command resolves to. Broken configuration puts
  `npu` in degraded mode: `--help` still works, every other command exits `2`
  with the offending file named
  ([`712229c`](https://github.com/fmatsos/npu/commit/712229c8cd1c2b8fb22dbb0f34abf329e3d29dc8))
- `npu serve`, `stop`, `status` and `logs` drive the container of a backend
  declaring a `[docker]` table — image, ports and accelerator come from that
  table, so switching runtime is a configuration change. Docker is optional:
  a machine that declares no container is never penalized
  ([`e0f2c5a`](https://github.com/fmatsos/npu/commit/e0f2c5acffebb5c6e659d2cb6b2c7f3bdaca822b))
- `--verbose error|warn|info`, global, default `warn`. Diagnostics go to
  stderr only; no level ever adds or removes a byte on stdout, which carries
  the command result and nothing else
  ([`e0f2c5a`](https://github.com/fmatsos/npu/commit/e0f2c5acffebb5c6e659d2cb6b2c7f3bdaca822b))
- Exit codes are the API for the programs driving `npu`: `1` I/O, `2` the
  user's configuration is wrong, `3` the backend is unreachable or answered
  non-2xx, `4` the answer violated the declared contract
  ([`b36ea22`](https://github.com/fmatsos/npu/commit/b36ea220357f0633e1a6f1b269c807272d8e7d2f),
  [`712229c`](https://github.com/fmatsos/npu/commit/712229c8cd1c2b8fb22dbb0f34abf329e3d29dc8))
- Five Claude Code skills — `npu-config`, `npu-backend`, `npu-model`,
  `npu-command`, `npu-doctor` — write and repair the configuration files an
  agent has to produce
  ([`29f7007`](https://github.com/fmatsos/npu/commit/29f7007008972f675bf14b7c19b8da6d4c431466),
  [`92af8b2`](https://github.com/fmatsos/npu/commit/92af8b2c3835a5c71c65e8960274557bcebb84aa),
  [`f7ac509`](https://github.com/fmatsos/npu/commit/f7ac50976a991dff1c378ad684a7d3cbaf280641))
- Prebuilt binaries for Linux (x86-64, ARM64), macOS (Intel, Apple silicon)
  and Windows (x86-64, ARM64), plus the Linux x86-64 triple built on Fedora
  and on Arch
  ([`03fd1bc`](https://github.com/fmatsos/npu/commit/03fd1bc19963100bfd5b18cc6ad721bc0aa64c42),
  [`61bf2dd`](https://github.com/fmatsos/npu/commit/61bf2dd3d57affe20fb17f4d7c0c89262e7eb933))

**Full changelog**: [`v0.1.0`](https://github.com/fmatsos/npu/commits/v0.1.0)

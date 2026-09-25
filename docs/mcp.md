# MCP server

`npu mcp serve` exposes configured business commands over stdio using `rmcp`. Configure an
MCP client to launch the `npu` executable with arguments `mcp`, `serve`, optionally preceded
by `--config-dir DIR`. Standard output contains only MCP messages; diagnostics go to stderr.

The server supports MCP `2026-07-28`: start with `server/discover`, then use `tools/list` and
`tools/call` with the required per-request `_meta`. Legacy `initialize` negotiation is not
supported. Only the `tools` capability is advertised. No MCP client, sampling, prompts,
resources, or hot reload is provided.

Each command path becomes a tool name by joining its segments with `_`. Name collisions are
fatal at startup and report both command files. Tool arguments come from typed `[args.*]`:
enums, bounded integers, strings, and file content. Send the command's main text as the
literal `"mcp.input"` property. It remains distinct from a configured `input` argument.
Unknown, missing, or incorrectly typed properties are rejected before a backend request.
A command with `[input] mode = "binary"` is not exposed: tool arguments are JSON, not bytes.

Successful calls return the finalized command output as text. JSON output is additionally
returned in `structuredContent`, always the whole document: `[output].extract` applies to the
CLI only. Pipeline failures have `isError: true` and a structured
error envelope in `structuredContent`. If configuration loading fails, discovery explains
the error and recommends `npu doctor`; `tools/list` is empty. Restart the server after
editing configuration.

Architecture decision: `rmcp` owns protocol framing and version validation, with Tokio only
at the transport boundary. The existing blocking business pipeline runs in a blocking worker,
with one execution at a time. This adds a dependency/runtime cost but avoids a hand-written
JSON-RPC implementation. MCP does not start runtimes: use `npu backend serve` explicitly.

---
description = "Generate a conventional commit message"
model = "qwen3-8b"

[input]
mode = "stdin"

[output]
format = "text"
max_lines = 1
---

Generate a Conventional Commit message from the supplied diff.

Return exactly one commit message.

Do not use Markdown.
Do not explain your answer.

{{ input }}

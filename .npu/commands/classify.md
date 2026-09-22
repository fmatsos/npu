---
description = "Classify an input document"
model = "qwen3-8b"

[input]
mode = "stdin_or_file"

[output]
format = "json"
schema = "schemas/classification.json"
---

You are a deterministic classifier.

Classify the following content.

Return only data matching the requested schema.

{{ input }}

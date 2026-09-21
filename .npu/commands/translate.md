+++
description = "Translate input text"
model = "qwen-fast"

[args.language]
short = "l"
required = true
description = "Target language"

[input]
mode = "stdin_or_file"
+++

Translate the following text into {{ args.language }}.

Preserve meaning and tone.

{{ input }}

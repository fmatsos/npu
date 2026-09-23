---
name: npu-discover
description: Searches Hugging Face for models the host's Intel NPU can actually run, and reports only those — filtered by the architectures `optimum-intel` exports to OpenVINO and, where checkable, by what the NPU plugin runs rather than silently falling back to CPU. Never returns an incompatible model padded with caveats; a model that fails the filter is left out, not listed with a warning. Use it before picking a model to export, when the choice of model is still open.
when_to_use: >
  Trigger on "what models can run on my NPU", "find a model for the NPU",
  "search Hugging Face for NPU-compatible models", "npu-discover", or any
  request to pick a model before exporting one, as opposed to a request that
  already names the model (which goes straight to npu-export).
argument-hint: "[optional: task or size hint, e.g. \"coding, under 4B\"]"
model: sonnet
effort: medium
allowed-tools: Read Grep Bash(npu model discover:*) Bash(npu describe:*)
---

# Finding models the host NPU can run

## Usage

```
/npu-discover [task or size hint]
```

e.g. `/npu-discover coding, under 4B parameters`, or bare `/npu-discover` for a general-purpose
chat model. This skill only searches and filters — it never exports. Hand the chosen id to
**npu-export** afterwards.

## 1. Run the search

The filtering is done by `npu` itself — do not re-implement it with `curl`:

```sh
npu model discover --npu [words] [-n 20] [--task text-generation] [--max-memory 50] [--candidates 100]
```

Turn the hint into search words (`qwen coder`, `phi`, `llama instruct`...) and, when it names a
task other than chat or code, the matching Hugging Face `--task`. A size hint is applied by reading
the `params` column, not by a flag.

For a backend already configured, `--backend <backend-id>` restricts the list to its engine
(`openvino`, `llamacpp`, `mlx`, also accepted by name); `--npu` is `--backend openvino` plus the
NPU check.

`--npu` is what makes the list NPU-specific: without it, the command lists whatever runs on the
host, CPU and GPU included. With it, a model is kept only when all of these hold. Never present a
dropped model as a near-miss:
- its architecture is exportable to OpenVINO for the task, according to `optimum-intel`'s
  registry read at run time;
- its `pipeline_tag` is the task;
- it is not already quantized;
- it has at least 100M parameters;
- it is not gated, unless `HF_TOKEN` is set;
- it fits the host: llmfit's `Perfect` or `Good` with a score of at least `--min-score` when
  llmfit is on `PATH` and knows the model, otherwise the INT4 weights within `--max-memory`
  percent of the RAM.

When `llmfit` is on `PATH`, it adds `score`, `fit`, `on` and `use case` columns.

Exit codes:
- `3`: either no Intel NPU (say so plainly and stop, since a list nobody can run is useless), or
  Hugging Face or the registry cannot be reached (report the URL it names);
- `0` with only the header line: nothing passed. Try broader words or `--candidates 300`, and
  say so rather than stretching the filter.

## 2. Report the survivors

Relay the table (model id, type, parameters, INT4 size, license, and llmfit's score and use case
when present) filtered by the user's hint. Prefer instruction-tuned variants (`-Instruct`, or a
chat template) for a chat or coding use, and mention the licence when it is not permissive.

Close with the next step: `/npu-export <chosen-id>`. It re-verifies the export concretely (the CPU
sanity check) rather than trusting this list's architecture match alone. This skill answers "worth
trying"; **npu-export**'s step 3 answers "actually works". After the export,
`npu backend tune` sizes its context.

## What this skill deliberately does not do

- **It does not export anything.**
- **It does not guarantee NPU correctness.** An exportable architecture can still quantize badly
  (see
  [docs/intel-npu.md §2](https://github.com/fmatsos/npu/blob/main/docs/intel-npu.md#2-choosing-quantization-parameters))
  or fall back from the NPU plugin to CPU for an unsupported op. Only an export and a test catch
  either.
- **It does not pad the list with near-misses.**

## Reference

- [`npu model discover`](https://github.com/fmatsos/npu/blob/main/docs/cli.md#npu-model-discover)
- [Deploying on an Intel NPU](https://github.com/fmatsos/npu/blob/main/docs/intel-npu.md)

Related skills: **npu-export** to actually produce and wire in the chosen model,
**npu-backend** if no NPU-targeting backend exists yet.

<!-- model/effort: judgement in choosing search words and reading the table, no irreversible action — sonnet/medium. -->

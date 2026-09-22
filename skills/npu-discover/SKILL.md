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
allowed-tools: Read Grep Bash(curl:*) Bash(python3:*) Bash(lspci:*) Bash(lsmod:*) Bash(free:*)
---

# Finding models the host NPU can run

## Usage

```
/npu-discover [task or size hint]
```

e.g. `/npu-discover coding, under 4B parameters`, or bare `/npu-discover` for a general-purpose
chat model. This skill only searches and filters — it never exports. Hand the chosen id to
**npu-export** afterwards.

## 0. Detect the host NPU first

Same check as **npu-export** — see
[Detecting an Intel NPU on the host](https://github.com/fmatsos/npu/blob/main/docs/intel-npu.md#0-detecting-an-intel-npu-on-the-host).
Searching for NPU-compatible models on a host with no Intel NPU produces a list nobody can use —
confirm the device first, and say so plainly if it's missing instead of searching anyway.

Also read available system RAM — a client Intel NPU shares it, it has no dedicated memory of its
own:

```sh
free -h
```

Use this as a soft ceiling on candidate size (an INT4 export needs roughly half a byte per
parameter, plus overhead and whatever else is running) rather than a fabricated fixed number — a
16 GB host and a 64 GB host do not have the same ceiling.

## 1. Get the current architecture support list — do not rely on memory

`optimum-intel`'s exportable architectures change between releases. Fetch it fresh via ctx7 before
filtering anything:

```
library: optimum-intel
query: "supported model architectures export openvino text generation"
```

This is the **first, and coarser, filter**: an architecture `optimum-intel` cannot export to
OpenVINO at all is excluded regardless of anything else. A model passing this filter is exportable
in principle — whether every op it uses also runs on the NPU plugin specifically (rather than
silently falling back to CPU inside OVMS) is checked in step 3, not guaranteed here.

## 2. Search Hugging Face

The public Hub API needs no authentication for a read-only search:

```sh
curl -s "https://huggingface.co/api/models?search=<query>&filter=text-generation&sort=downloads&direction=-1&limit=50" \
    | python3 -m json.tool
```

Adjust `filter` for the task implied by the hint (`text-generation` is the common case for a chat
or coding model; use the task the user actually asked for). Pull enough candidates (`limit=50` or
more) that the architecture filter in step 3 still leaves a useful shortlist — most results at this
stage are not yet checked for exportability.

## 3. Filter to what is actually exportable

For each candidate, in descending popularity order until enough compatible ones are found:

```sh
curl -s "https://huggingface.co/<model-id>/raw/main/config.json" \
    | python3 -c "import json,sys; d=json.load(sys.stdin); print(d.get('model_type'), d.get('num_parameters', d.get('num_hidden_layers')))"
```

Keep a candidate only if **all** of these hold:

- `model_type` is on the list fetched in step 1;
- it is not gated (or `HF_TOKEN` is set — check `curl -s .../api/models/<id> | ... .get('gated')`);
- its size fits the RAM ceiling from step 0 with headroom for the OS, OVMS, and `npu` itself —
  don't cut it exactly at the limit.

Discard everything else **without listing it** — the point of this skill is a clean list of things
that will work, not a longer list with asterisks.

## 4. Report only the survivors

For each compatible model: its Hugging Face id, `model_type`, approximate parameter count, and
license. Nothing else needs justifying per-row — passing the filter already said why it's there.
State plainly if a decent hint returns nothing rather than stretching the filter to force a result.

Close with the next step: `/npu-export <chosen-id>` — which re-verifies the export concretely
(the CPU sanity check) rather than trusting this list's architecture match alone. This skill
answers "worth trying"; **npu-export**'s step 3 answers "actually works".

## What this skill deliberately does not do

- **It does not export anything.** Searching and filtering only — no venv, no `optimum-cli`, no
  disk usage beyond the HTTP responses it reads.
- **It does not guarantee NPU correctness.** An architecture on the `optimum-intel` export list can
  still produce a broken quantization (see the pitfall in
  [docs/intel-npu.md §2](https://github.com/fmatsos/npu/blob/main/docs/intel-npu.md#2-choosing-quantization-parameters))
  or fall back silently from the NPU plugin to CPU for an unsupported op. Both are only caught by
  actually exporting and testing — this skill narrows the search, it does not replace verification.
- **It does not pad the list with near-misses.** A model failing the filter is left out, not
  appended with a caveat — that defeats the purpose of asking for "only those".

## Reference

This skill is a summary. When a case is not covered here, the repository documentation and the
current `optimum-intel` documentation (fetched via ctx7, not memorized) are authoritative:

- [Deploying on an Intel NPU](https://github.com/fmatsos/npu/blob/main/docs/intel-npu.md)
- [Hugging Face Hub API](https://huggingface.co/docs/hub/api) for the search endpoint's parameters

Related skills: **npu-export** to actually produce and wire in the chosen model,
**npu-backend** if no NPU-targeting backend exists yet.

<!-- model/effort: judgement in choosing search terms and reading architecture-support docs, but no irreversible action — sonnet/medium. -->

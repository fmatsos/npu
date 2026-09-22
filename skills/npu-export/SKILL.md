---
name: npu-export
description: Exports a Hugging Face model for the host's Intel NPU with `optimum-cli`/`optimum-intel`, verifies the export is coherent before reporting success, and writes the matching `npu` model file so it can be used immediately. Covers detecting whether the host actually has an Intel NPU, the quantization flags that compile correctly on it, the CPU sanity check that catches a broken export before it ever reaches a backend, and which configuration scope the generated model file belongs in. Use it whenever a model needs to run on the host's NPU and has no local OpenVINO export yet.
when_to_use: >
  Trigger on "export this model for the NPU", "get <model> running on my
  NPU", "quantize <model> for OpenVINO", "npu-export", or any request to
  prepare a Hugging Face model to be served on an Intel NPU through OVMS.
argument-hint: "[huggingface model id]"
model: sonnet
effort: medium
allowed-tools: Read Write Edit Glob Grep Bash(npu:*) Bash(python3:*) Bash(pip:*) Bash(optimum-cli:*) Bash(curl:*) Bash(lspci:*) Bash(lsmod:*)
---

# Exporting a Hugging Face model for the host NPU

## Usage

```
/npu-export <huggingface-model-id>
```

e.g. `/npu-export Qwen/Qwen2.5-Coder-3B-Instruct`. The argument is a Hugging Face repo id, not a
local path — **npu-discover** lists ids already known to be compatible when one has not been
picked yet.

This skill is the executor for
[`docs/intel-npu.md`](https://github.com/fmatsos/npu/blob/main/docs/intel-npu.md); it does not
duplicate the reasoning there, it runs the procedure and reports the result.

## 0. Confirm there is an Intel NPU to export for

`optimum-intel` is Intel-specific; exporting for a device that is not there wastes time on
something nobody can use.

```sh
ls /dev/accel/accel0 2>/dev/null && echo present
lspci -nn 2>/dev/null | grep -i "neural\|npu\|vpu"
lsmod 2>/dev/null | grep -i intel_vpu
```

No device node: say so plainly and stop rather than exporting blindly — see
[Detecting an Intel NPU on the host](https://github.com/fmatsos/npu/blob/main/docs/intel-npu.md#0-detecting-an-intel-npu-on-the-host)
for the driver-vs-hardware distinction before concluding "no NPU". If the user explicitly wants a
GPU/CPU export instead, the export procedure below is identical — only the OVMS `--target_device`
in the generated backend changes, and that is **npu-backend**'s job, not this skill's.

## 1. Confirm the model can actually be exported

```sh
curl -s "https://huggingface.co/api/models/<model-id>" | python3 -c "import json,sys; d=json.load(sys.stdin); print(d.get('gated'), d.get('pipeline_tag'))"
curl -s "https://huggingface.co/<model-id>/raw/main/config.json" | python3 -c "import json,sys; print(json.load(sys.stdin).get('model_type'))"
```

- `gated` true and no `HF_TOKEN` in the environment: stop and say so — do not attempt a download
  that will 401.
- The architecture (`model_type`) must be one `optimum-intel` actually exports to OpenVINO for
  the model's task. This list changes as `optimum-intel` releases — **check it fresh via ctx7**
  (`optimum-intel`, query: "supported model architectures export openvino") rather than trusting
  memory. An architecture not on that list: stop, name it, do not attempt the export — a failed
  `optimum-cli` run after a multi-gigabyte download is a worse failure mode than not starting.

## 2. Export

Reuse the venv if it already exists — installing `optimum-intel[openvino]` fresh takes minutes:

```sh
[ -d ~/.cache/npu-ov-export ] || python3 -m venv ~/.cache/npu-ov-export
source ~/.cache/npu-ov-export/bin/activate
pip show optimum-intel >/dev/null 2>&1 || pip install "optimum-intel[openvino]"

optimum-cli export openvino \
    -m <model-id> \
    --weight-format int4 --sym --ratio 1.0 --group-size 128 \
    ~/models/<basename>-int4-ov
```

`<basename>` is the last path segment of `<model-id>`, lowercased, `/` dropped. **`--sym` and
`--group-size 128` are not negotiable defaults** — `--group-size -1` compiles and serves fine and
silently produces degenerate output; see the pitfall documented in
[docs/intel-npu.md §2](https://github.com/fmatsos/npu/blob/main/docs/intel-npu.md#2-choosing-quantization-parameters).
Lower `--ratio` (e.g. `0.8`) only if step 3 below shows a quality problem `--group-size 128`
doesn't fix — it trades export size for accuracy, so it is a fallback, not a default.

## 3. Verify before reporting success

Never report an export as done without running this. It is seconds on CPU and it is the only
thing that catches a broken quantization before it reaches OVMS:

```sh
python3 -c "
from optimum.intel import OVModelForCausalLM
from transformers import AutoTokenizer

path = '<export-dir>'
tok = AutoTokenizer.from_pretrained(path)
model = OVModelForCausalLM.from_pretrained(path, device='CPU')
messages = [{'role': 'user', 'content': 'Say hello in one short sentence.'}]
prompt = tok.apply_chat_template(messages, tokenize=False, add_generation_prompt=True)
inputs = tok(prompt, return_tensors='pt')
out = model.generate(**inputs, max_new_tokens=40, do_sample=False)
print(tok.decode(out[0], skip_special_tokens=True))
"
```

Coherent prose: proceed. A single token or short pattern repeating past a handful of times
(`2\n2\n2\n...`, `!!!!!!!`): the export is broken — do not wire it into `npu`. Retry step 2 with
`--ratio 0.8`; if that still fails, the architecture is a poor fit for this quantization regardless
of what step 1 said, and that is worth reporting as-is rather than guessing further.

## 4. Generate the `npu` model file

Only after step 3 passes. The exported directory name is the `model` field:

```toml
# ~/.config/npu/models/<id>.toml
id = "<id>"
backend = "<existing-ovms-style-backend-id>"
operation = "chat"
model = "<basename>-int4-ov"

[generation]
temperature = 0.0
```

**Write it to the user scope (`$XDG_CONFIG_HOME/npu`, usually `~/.config/npu`), not the project's
`./.npu`, unless the user explicitly asks otherwise.** The export is tied to this machine's NPU and
its local `~/models` path — committing that into a project's `.npu/` would break the next person
who runs it without this hardware. See **npu-config** for the scope reasoning in full.

This requires a backend already declaring `[docker]` with `--target_device NPU` (or GPU/CPU, per
step 0). If none exists, say so and point at **npu-backend** instead of fabricating one — the
`[docker]` table needs host-specific values (render group id, uid/gid, the NPU device path) this
skill has no way to guess correctly.

Re-exporting an existing model **in place** (same directory) invalidates its OVMS compilation
cache silently — clear the stale blob per
[docs/intel-npu.md §4](https://github.com/fmatsos/npu/blob/main/docs/intel-npu.md#4-persisting-the-compilation-cache)
before the next `npu serve`.

## 5. Report

State plainly:

- the `model` value to use (`<basename>-int4-ov`) and its full path;
- whether a model file was written, and in which scope;
- the next step: `npu serve <id>`, then `npu doctor` to confirm resolution, then a real request
  through the command that will use it.

## What this skill deliberately does not do

- **It does not start or test the container.** `npu serve` needs a backend whose `[docker]` table
  already targets the right device — that is configuration, not export.
- **It does not touch `npu`'s Rust source.** The export, the quantization choice and the generated
  TOML are all external to the binary — consistent with `npu` knowing nothing about specific
  models or hardware.
- **It does not overwrite an export at an existing path silently.** Ask first; re-exporting in
  place is a deliberate replacement, not a default.

## Reference

This skill is a summary. When a case is not covered here, or when the behaviour it describes does
not match what actually happens, the repository documentation is authoritative:

- [Deploying on an Intel NPU](https://github.com/fmatsos/npu/blob/main/docs/intel-npu.md) — the
  full procedure this skill executes, including the quantization pitfall and troubleshooting table
- [Configuration scopes](https://github.com/fmatsos/npu/blob/main/docs/configuration.md#scopes-and-precedence)

Related skills: **npu-discover** to pick a model before exporting one, **npu-backend** for the
`[docker]` table this skill's model depends on, **npu-model** for the file format it generates,
**npu-doctor** to diagnose the result.

<!-- model/effort: a fixed procedure with one judgement call (reading the sanity-check output) and external, sometimes slow, commands — sonnet/medium, not the high bar of a diagnosis or a release. -->

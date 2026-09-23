---
name: npu-export
description: Exports a Hugging Face model for the host's Intel NPU with `optimum-cli`/`optimum-intel`, verifies the export is coherent, builds the GPU twin the `fallback` key needs, and writes both `npu` model files so the pair can be used immediately. Covers detecting whether the host actually has an Intel NPU, the quantization flags that compile correctly on it, the CPU sanity check that catches a broken export before it ever reaches a backend, why the target device is baked into each export directory rather than chosen per request, and which configuration scope the generated model files belong in. Use it whenever a model needs to run on the host's NPU and has no local OpenVINO export yet.
when_to_use: >
  Trigger on "export this model for the NPU", "get <model> running on my
  NPU", "quantize <model> for OpenVINO", "npu-export", or any request to
  prepare a Hugging Face model to be served on an Intel NPU through OVMS —
  including "set up the GPU fallback for <model>".
argument-hint: "[huggingface model id]"
model: sonnet
effort: medium
allowed-tools: Read Write Edit Glob Grep Bash(npu:*) Bash(python3:*) Bash(pip:*) Bash(optimum-cli:*) Bash(curl:*) Bash(lspci:*) Bash(lsmod:*) Bash(chmod:*) Bash(docker:*)
---

# Exporting a Hugging Face model for the host NPU, with its GPU twin

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
GPU/CPU export instead, `optimum-cli` in step 2 is identical — only the `--target_device` passed
to `--configure` (step 3.5) changes, since that is what bakes the device into the export. Step 3.6,
which builds the GPU twin, is then pointless and must be skipped.

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

## 3.5. Generate the OVMS `graph.pbtxt`

A directory produced by `optimum-cli export openvino` is a bare OpenVINO IR — it has no
`graph.pbtxt`. OVMS's `--source_model` serve path only *reads* an existing `graph.pbtxt`; unlike a
pull from the Hub, it never generates one for a local export, and serving fails immediately with
`Unable to open file: <path>/graph.pbtxt` (`npu backend serve` then reports a non-zero backend exit and
`npu doctor` shows the backend unreachable). Generate it explicitly, once, right after step 3
passes:

```sh
docker run --rm -v ~/models:/models:rw openvino/model_server:2026.4.0 \
    --configure --model_path /models/<basename>-int4-ov \
    --task text_generation --target_device NPU
```

**`--target_device` here is not a hint, it is the device.** `--configure` writes it into
`graph.pbtxt` as `device: "NPU"`, and that is what OVMS reads at serve time — a backend serving
this directory with `--model_name`/`--model_path` passes no device flag at all. One export
directory therefore serves exactly one device, which is why step 3.6 exists.

Match the OVMS image tag to the one the backend's `[docker]` table uses: a mismatch pulls a second
multi-GB image for nothing.

**If this prints `Unable to open file: .../graph.pbtxt` again instead of `Graph: graph.pbtxt
created in: ...`, it is a permissions problem, not a missing-file problem** — `--configure` is
trying to *create* the file and can't. The image runs as a fixed non-root user (`ovms`, uid 5000)
inside the container; the export directory is owned by the host user who ran `optimum-cli` and is
usually not writable by anyone else:

```sh
ls -ld ~/models/<basename>-int4-ov      # confirm: no write bit for "other"
chmod o+w ~/models/<basename>-int4-ov ~/models/.ov_cache
```

Re-run the `--configure` command above after fixing permissions. This is the same class of problem
as the documented `Cache directory /cache is not writable` warning in
[docs/intel-npu.md's troubleshooting table](https://github.com/fmatsos/npu/blob/main/docs/intel-npu.md#troubleshooting)
— add a row for it there if it is still missing.

## 3.6. Build the GPU twin

**Always, not on request.** An NPU-compiled graph has a static maximum prompt length: a prompt
past it is refused with `400 ... Input length exceeds the maximum allowed length`. `npu`'s
`fallback` key recovers from exactly that by retrying on a GPU-served model — but only if one
exists. Exporting for the NPU alone leaves the fallback permanently unavailable, and the failure
then surfaces to the user instead of being absorbed.

The twin is **not a second export**. It is a directory of symlinks plus its own `graph.pbtxt`, the
one file that differs — a few kilobytes against several gigabytes. OVMS resolves the graph's
`models_path: "."` relative to the graph itself, and the whole `~/models` tree is bind-mounted, so
the symlinks resolve inside the container:

```sh
mkdir -p ~/models/<basename>-int4-ov-gpu
cd ~/models/<basename>-int4-ov-gpu
for f in ../<basename>-int4-ov/*; do
    [ "$(basename "$f")" = graph.pbtxt ] || ln -sf "$f" .
done
chmod o+w .    # same uid-5000 constraint as step 3.5

docker run --rm -v ~/models:/models:rw openvino/model_server:2026.4.0 \
    --configure --model_path /models/<basename>-int4-ov-gpu \
    --task text_generation --target_device GPU
```

Confirm `device: "GPU"` in the generated `~/models/<basename>-int4-ov-gpu/graph.pbtxt` before
moving on — a twin that silently says `NPU` is a fallback that cannot help.

Skip this step **only** when step 0 found no NPU and the export was already targeting the GPU:
there is nothing to fall back from. Say so rather than building a twin of a GPU export.

## 4. Generate the `npu` model files

Two files, not one — a primary on the NPU declaring the fallback, and the GPU twin it points at.
Only after step 3.6 passes:

```toml
# ~/.config/npu/models/<id>.toml
id = "<id>"
backend = "<npu-backend-id>"
operation = "chat"
model = "<basename>-int4-ov"
fallback = "<id>-gpu"

[generation]
temperature = 0.0
```

```toml
# ~/.config/npu/models/<id>-gpu.toml
id = "<id>-gpu"
backend = "<gpu-backend-id>"
operation = "chat"
model = "<basename>-int4-ov-gpu"

[generation]
temperature = 0.0
```

No `fallback` on the twin: the retry is single hop, so a chain would not be followed anyway.

**Write both to the user scope (`$XDG_CONFIG_HOME/npu`, usually `~/.config/npu`), not the
project's `./.npu`, unless the user explicitly asks otherwise.** The export is tied to this
machine's NPU and its local `~/models` path — committing that into a project's `.npu/` would break
the next person who runs it without this hardware. See **npu-config** for the scope reasoning in
full.

This needs **two** backends, each with its own `[docker]` table, its own port and its own
container: the device is baked into the served export, and `npu` names a container
`npu-<backend-id>`, so one backend can hold exactly one running model. If either is missing, say
so and point at **npu-backend** instead of fabricating one — a `[docker]` table needs
host-specific values (render group id, uid/gid, device paths) this skill has no way to guess
correctly.

Two coupling facts worth stating when reporting:

- Re-exporting **in place** (same directory) invalidates the OVMS compilation cache silently —
  clear the stale blob per
  [docs/intel-npu.md §4](https://github.com/fmatsos/npu/blob/main/docs/intel-npu.md#4-persisting-the-compilation-cache)
  before the next `npu backend serve` — **and** it changes the twin too, whose `graph.pbtxt` then describes
  weights that no longer exist. Re-run step 3.6's `--configure` after any re-export.
- The twin shares the primary's `config.json`, hence its context length. It lifts the NPU's
  compiled prompt shape, never the model's own context ceiling: a prompt past that fails on both,
  with a different message (`Number of prompt tokens: N exceeds model max length: M`).

## 5. Report

State plainly:

- the two `model` values (`<basename>-int4-ov` and `<basename>-int4-ov-gpu`) and their full paths,
  and that the twin is symlinks, not a second copy of the weights;
- that both `graph.pbtxt` files were generated with the right `device:` (steps 3.5 and 3.6) and,
  if permissions had to be fixed, that they were;
- whether the two model files were written, in which scope, and that `<id>` declares
  `fallback = "<id>-gpu"`;
- the next step: `npu backend serve <id>` **and** `npu backend serve <id>-gpu` (two containers, two ports), then
  `npu doctor` to confirm both backends resolve, `npu config models` to see the `FALLBACK` column, then a
  real request through the command that will use it.

## What this skill deliberately does not do

- **It does not start or test the actual serving containers.** `npu backend serve` needs backends whose
  `[docker]` tables already exist — that is configuration, not export. The `--configure` runs in
  steps 3.5 and 3.6 are preparation (they write `graph.pbtxt` and exit), not served containers, the
  same distinction OVMS itself draws between `--configure`/`--pull` and plain serve.
- **It does not write the backends.** It produces two model files pointing at an NPU backend and a
  GPU backend that must already exist; **npu-backend** owns those.
- **It does not touch `npu`'s Rust source.** The export, the quantization choice and the generated
  TOML are all external to the binary — consistent with `npu` knowing nothing about specific
  models or hardware.
- **It does not overwrite an export at an existing path silently.** Ask first; re-exporting in
  place is a deliberate replacement, not a default — and it invalidates both the compilation cache
  and the GPU twin.
- **It does not export twice.** The GPU twin shares the NPU export's weights through symlinks; a
  second `optimum-cli` run would spend gigabytes producing an identical IR.

## Reference

This skill is a summary. When a case is not covered here, or when the behaviour it describes does
not match what actually happens, the repository documentation is authoritative:

- [Deploying on an Intel NPU](https://github.com/fmatsos/npu/blob/main/docs/intel-npu.md) — the
  full procedure this skill executes, including the quantization pitfall and troubleshooting table
- [Configuration scopes](https://github.com/fmatsos/npu/blob/main/docs/configuration.md#scopes-and-precedence)

Related skills: **npu-discover** to pick a model before exporting one, **npu-backend** for the two
`[docker]` tables this skill's models depend on, **npu-model** for the file format it generates and
the `fallback` semantics, **npu-doctor** to diagnose the result.

<!-- model/effort: a fixed procedure with one judgement call (reading the sanity-check output) and external, sometimes slow, commands — sonnet/medium, not the high bar of a diagnosis or a release. -->

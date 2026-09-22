# Deploying a model on an Intel NPU

- [Overview](#overview)
- [0. Detecting an Intel NPU on the host](#0-detecting-an-intel-npu-on-the-host)
- [1. Export the model with `optimum-cli`](#1-export-the-model-with-optimum-cli)
- [2. Choosing quantization parameters](#2-choosing-quantization-parameters)
- [3. Serving the export with OVMS on the NPU](#3-serving-the-export-with-ovms-on-the-npu)
- [4. Persisting the compilation cache](#4-persisting-the-compilation-cache)
- [5. Wiring it into `npu`](#5-wiring-it-into-npu)
- [Troubleshooting](#troubleshooting)

---

## Overview

`npu` drives any OpenAI-compatible backend; it knows nothing about OpenVINO, NPUs or
quantization. This guide covers the part outside `npu`'s scope: producing a model export that
compiles and runs correctly on an Intel NPU through
[OpenVINO Model Server](https://github.com/openvinotoolkit/model_server) (OVMS), using
[`optimum-intel`](https://github.com/huggingface/optimum-intel)'s `optimum-cli`.

The pipeline has three independent stages, and a problem in any of them looks like a problem in
the next one — worth isolating before assuming `npu` or OVMS is at fault:

```text
optimum-cli export  →  OVMS serves the export  →  npu sends chat requests to OVMS
     (export bug)          (deployment bug)              (client bug)
```

---

## 0. Detecting an Intel NPU on the host

Everything below is Intel-specific: `optimum-intel` targets Intel hardware, and `--target_device
NPU` in OVMS means nothing on a machine without one. Confirm the device exists before spending
time on an export:

```sh
ls /dev/accel/accel0 2>/dev/null && echo "NPU device node present"
lspci -nn 2>/dev/null | grep -i "neural\|npu\|vpu"
lsmod 2>/dev/null | grep -i intel_vpu
```

The Linux driver (`intel_vpu`, kernel 6.5+ or the out-of-tree module on older kernels) exposes the
NPU as `/dev/accel/accel0`. Its absence with the kernel module loaded usually means firmware is
missing (`intel-driver-compiler-npu` / `linux-firmware`, depending on the distribution) rather than
missing hardware — a Meteor Lake, Lunar Lake, Arrow Lake or Panther Lake CPU (marketed as "Intel AI
Boost") that doesn't expose the device node is a driver problem, not an absent NPU.

No device node, no driver, no fix in reach: fall back to `--target_device GPU` or `CPU` in the
OVMS `args` — everything else in this guide (the export, `--cache_dir`, the backend wiring)
applies identically, only the device flag changes.

---

## 1. Export the model with `optimum-cli`

```sh
python3 -m venv ~/.cache/npu-ov-export
source ~/.cache/npu-ov-export/bin/activate
pip install "optimum-intel[openvino]"

optimum-cli export openvino \
    -m Qwen/Qwen2.5-Coder-3B-Instruct \
    --weight-format int4 --sym --ratio 1.0 --group-size 128 \
    ~/models/Qwen2.5-Coder-3B-Instruct-int4-ov
```

The output directory is a self-contained OpenVINO IR: `openvino_model.xml/.bin`, tokenizer and
detokenizer IR, `chat_template.jinja`, `config.json`. OVMS reads it directly — nothing further to
convert.

A model already published on the Hugging Face Hub in OpenVINO IR form (search for an `-ov` or
`-openvino` suffix) skips this step, but its quantization settings are whatever the publisher
chose; export your own when the NPU needs specific settings, which it usually does (see below).

---

## 2. Choosing quantization parameters

| Flag | Meaning | NPU guidance |
| --- | --- | --- |
| `--weight-format int4` | 4-bit weight-only quantization | required for a 3-8B model to compile and run at usable speed on an NPU |
| `--sym` | symmetric quantization | **required for the NPU**: asymmetric quantization compiles on the NPU but takes dramatically longer (minutes, vs. seconds for symmetric) — the NPU compiler's static-shape graph optimizer handles symmetric ranges far better |
| `--ratio` | fraction of layers kept in int4 vs. int8 | `1.0` = everything int4 (smallest, fastest to compile, lowest quality floor); lower it (e.g. `0.8`) if the int4 output is noticeably worse than expected and you can accept a larger export |
| `--group-size` | width of each quantization group | **use `128`, never `-1`** — see the warning below |

> [!WARNING]
> **`--group-size -1` degrades quantization to per-column (per-channel) granularity — the
> coarsest possible — and combined with `--sym --ratio 1.0` it can produce a model that compiles
> and serves without any error, but generates degenerate output** (repeating a single token,
> looping on garbage) for every prompt. There is no NPU requirement for `-1`; it does not
> compile faster or run faster than `128`, it is simply a much larger accuracy loss. `128` is
> `optimum-intel`'s own documented default and the safe choice.
>
> This is not a hypothetical: exporting `Qwen2.5-Coder-3B-Instruct` with `--group-size -1` produced
> a model that answered every prompt with `"2\n2\n2\n..."`, confirmed to be the model itself —
> not `npu`, not OVMS — by running the export directly through `optimum-intel` on CPU, bypassing
> both. Re-exporting with `--group-size 128` (same `--sym --ratio 1.0` otherwise) fixed it.

### Verifying an export before deploying it

Before pointing OVMS at a new export, sanity-check it directly with `optimum-intel` on CPU. This
isolates the export itself from OVMS and the NPU — if the model is broken here, redeploying it
changes nothing:

```sh
source ~/.cache/npu-ov-export/bin/activate
python3 -c "
from optimum.intel import OVModelForCausalLM
from transformers import AutoTokenizer

path = '~/models/Qwen2.5-Coder-3B-Instruct-int4-ov'
tok = AutoTokenizer.from_pretrained(path)
model = OVModelForCausalLM.from_pretrained(path, device='CPU')

messages = [{'role': 'user', 'content': 'Write a bubble sort function in Python.'}]
prompt = tok.apply_chat_template(messages, tokenize=False, add_generation_prompt=True)
inputs = tok(prompt, return_tensors='pt')
out = model.generate(**inputs, max_new_tokens=120, do_sample=False)
print(tok.decode(out[0], skip_special_tokens=True))
"
```

Coherent output here means the export is sound; anything wrong afterwards is in the OVMS
deployment or the `npu` backend configuration, not the model.

---

## 3. Serving the export with OVMS on the NPU

```sh
docker run -d --name ovms -p 8000:8000 \
    -v ~/models:/models:rw \
    -v ~/models/.ov_cache:/cache:rw \
    --device /dev/accel --device /dev/dri --group-add <render-gid> --user <uid>:<gid> \
    openvino/model_server:2026.4.0-gpu \
    --source_model Qwen2.5-Coder-3B-Instruct-int4-ov \
    --model_repository_path /models \
    --rest_port 8000 \
    --target_device NPU \
    --cache_dir /cache
```

| Flag | Why |
| --- | --- |
| `--target_device NPU` | without it OVMS defaults to CPU and never touches the accelerator — the request still succeeds, just slowly and off the NPU, which is easy to miss |
| `--device /dev/accel` | exposes the NPU device node inside the container |
| `--device /dev/dri` + `--group-add <render-gid>` | needed alongside `/dev/accel` on most setups for the accelerator to actually be detected — omitting them shows `Available devices: CPU` in the logs, not an error |
| `--user <uid>:<gid>` | the container must run as a user with access to `/dev/accel`; running as root inside the container is not enough if the host device node is group-restricted |
| the `-gpu` image tag | there is no NPU-only OVMS image; the tag containing the GPU plugins also contains the NPU one |

Pin the image to an exact version (`2026.4.0-gpu`, not `latest-gpu`). A silent version bump
changes the OpenVINO runtime and invalidates every cached compilation (see next section) without
any visible signal beyond "serve got slow again".

---

## 4. Persisting the compilation cache

NPU graph compilation (converting the IR to the NPU's static-shape executable form) is the slow
part — tens of seconds to several minutes depending on quantization, model size and whether
`--sym` was used — and it reruns on every container start unless cached.

`--cache_dir /cache`, backed by a bind-mounted host directory, persists the compiled blob across
restarts. The cache key is implicitly {OVMS/OpenVINO version, target device, model weights,
shape parameters} — changing any of them (including re-exporting the model with different
weights, even to the same directory name) invalidates the cache silently. When re-exporting a
model in place, clear the stale blob explicitly rather than trusting the cache to detect the
change:

```sh
rm -f ~/models/.ov_cache/*.blob ~/models/.ov_cache/*.cl_cache
```

---

## 5. Wiring it into `npu`

None of the above is `npu`-specific — it is plain OVMS and Docker configuration, declared in a
backend's `[docker]` table exactly as documented in
[Starting a backend with Docker](configuration.md#starting-a-backend-with-docker):

```toml
# backends/ovms.toml
[docker]
image = "openvino/model_server:2026.4.0-gpu"
options = [
    "-p", "8000:8000",
    "-v", "{{ env.HOME }}/models:/models:rw",
    "-v", "{{ env.HOME }}/models/.ov_cache:/cache:rw",
    "--device", "/dev/accel", "--device", "/dev/dri",
    "--group-add", "<render-gid>", "--user", "<uid>:<gid>",
]
args = [
    "--source_model", "{{ args.model }}",
    "--model_repository_path", "/models",
    "--rest_port", "8000",
    "--target_device", "NPU",
    "--cache_dir", "/cache",
]
```

`{{ args.model }}` resolves to the model's `model` field — the export directory name under
`/models` — so `npu serve <model-id>` starts OVMS pointed at the right export without any
NPU-specific logic in `npu` itself. A slow accelerator can also need more time than the client's
default request timeout; see `[timeouts]` in
[Configuration](configuration.md#timeouts-optional).

---

## Troubleshooting

| Symptom | Likely stage | Check |
| --- | --- | --- |
| Coherent-looking startup, but every response is repetitive garbage | export | run the CPU verification in [§2](#verifying-an-export-before-deploying-it); if it reproduces there, the export is broken, not the deployment |
| `curl` straight to OVMS reproduces the same garbage as through `npu` | not `npu` | the client did its job faithfully; look at the export and the OVMS logs, not `backend.rs` |
| OVMS logs show `Available devices: CPU` | deployment | `--device`/`--group-add`/`--user` are missing or wrong; the NPU was never reached |
| `Cache directory /cache is not writable` | deployment | the container's `--user` cannot write the bind-mounted cache directory |
| `Unable to open file: .../graph.pbtxt` right after `npu serve` starts | deployment | a locally `optimum-cli`-exported directory has no `graph.pbtxt` — unlike a Hub pull, OVMS never generates one for a local export on its own; run `ovms --configure --model_path <dir> --task text_generation --target_device <device>` once. The same message when running `--configure` itself means it cannot *create* the file: the container runs as a fixed non-root user (`ovms`, uid 5000) and the export directory is usually only writable by the host user who ran `optimum-cli` — `chmod o+w` the export directory (and `.ov_cache`) and retry |
| Compilation takes minutes instead of seconds | export | the export is asymmetric (`--sym` missing) or the OVMS/image version changed and invalidated the cache |
| Fast on one machine, minutes on another for the "same" model | deployment | a floating image tag (`:latest-gpu`) resolved to a different OpenVINO version; pin the tag |

Each row is independently verifiable — reproduce with `curl` to rule `npu` in or out, reproduce on
CPU with `optimum-intel` to rule the export in or out, and check the OVMS container logs before
assuming either.

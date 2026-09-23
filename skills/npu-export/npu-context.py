#!/usr/bin/env python3
"""Size the NPU context of every exported model from the model and the host.

An NPU graph is compiled for a STATIC context: `MAX_PROMPT_LEN` tokens of
prompt plus `MIN_RESPONSE_LEN` tokens of answer. Anything longer is refused
(prompt) or cut (answer). This script picks both values, and the matching
`max_tokens` of the npu model files, from:

- the model: its maximum context (`max_position_embeddings`) and its KV cache
  cost per token (2 x layers x KV heads x head dim x 2 bytes, fp16);
- the host: total RAM, of which `--ram-share` is granted to the NPU models,
  minus their weights, split evenly between them so they can all run at once.

Usage:
    npu-context.py            # print the plan
    npu-context.py --apply    # write graph.pbtxt and the model files
    npu-context.py --self-test

Stdlib only (Python 3.11+ for tomllib).
"""

import argparse
import json
import os
import re
import sys
import tomllib
from pathlib import Path

# ponytail: an NPU graph costs, per token of context, its KV cache PLUS about
# ACTIVATION_BYTES x hidden_size x layers of static buffers — the second term
# dominates on models with few KV heads. Fitted on a Meteor Lake NPU (Core
# Ultra 7 165U, OVMS 2026.4, int4), container memory minus weights:
#   Qwen3-4B      7K 7.3 GB, 16K 13.1 GB, 32K 33.6 GB
#   Qwen3-8B      7K 10.6 GB
#   Coder-7B     16K 13.4 GB
#   Coder-3B     20K 11.0 GB
# The formula lands within +0..+15 % of each: never under. Recalibrate on
# another NPU or OVMS version.
ACTIVATION_BYTES = 6.2
ACTIVATION_BYTES_LONG = 9.0  # past LONG_CONTEXT tokens (measured at 32K)
LONG_CONTEXT = 24576

# Share of the context reserved for the answer; the rest is the prompt.
RESPONSE_SHARE = 0.25
# Contexts are rounded down to this step, and never go below it.
STEP = 1024


def kv_bytes_per_token(config: dict) -> int:
    """fp16 KV cache bytes per token, from a Hugging Face config.json."""
    heads = config["num_attention_heads"]
    head_dim = config.get("head_dim") or config["hidden_size"] // heads
    kv_heads = config.get("num_key_value_heads") or heads
    return 2 * config["num_hidden_layers"] * kv_heads * head_dim * 2


def activation_width(config: dict) -> int:
    """hidden_size x layers: what the per-token static buffers scale with."""
    return config["hidden_size"] * config["num_hidden_layers"]


def real_bytes(tokens: int, kv_per_token: int, width: int) -> float:
    """Estimated NPU memory for a `tokens` context, weights excluded."""
    factor = ACTIVATION_BYTES if tokens <= LONG_CONTEXT else ACTIVATION_BYTES_LONG
    return tokens * (kv_per_token + factor * width)


def plan_context(model_max: int, kv_per_token: int, width: int, budget_bytes: float) -> tuple[int, int]:
    """(MAX_PROMPT_LEN, MIN_RESPONSE_LEN) fitting both the model and the budget."""
    total = STEP
    candidate = STEP
    while candidate <= model_max and real_bytes(candidate, kv_per_token, width) <= budget_bytes:
        total = candidate
        candidate += STEP
    response = max(STEP // 2, int(total * RESPONSE_SHARE) // 256 * 256)
    return total - response, response


def mem_total_bytes() -> int:
    for line in Path("/proc/meminfo").read_text().splitlines():
        if line.startswith("MemTotal:"):
            return int(line.split()[1]) * 1024
    raise SystemExit("npu-context: MemTotal not found in /proc/meminfo")


def npu_models(config_dir: Path, models_dir: Path) -> list[dict]:
    """npu model files whose export directory holds an NPU graph."""
    found = []
    for path in sorted((config_dir / "models").glob("*.toml")):
        spec = tomllib.loads(path.read_text())
        export = models_dir / spec["model"]
        graph = export / "graph.pbtxt"
        if graph.is_file() and 'device: "NPU"' in graph.read_text():
            found.append({"file": path, "spec": spec, "export": export, "graph": graph})
    return found


def set_plugin_lengths(graph_text: str, prompt: int, response: int) -> str:
    """Set MAX_PROMPT_LEN / MIN_RESPONSE_LEN at the root of plugin_config."""
    match = re.search(r"plugin_config: '(\{.*?\})'", graph_text)
    if not match:
        raise ValueError("no plugin_config in graph.pbtxt")
    config = json.loads(match.group(1))
    config["MAX_PROMPT_LEN"] = prompt
    config["MIN_RESPONSE_LEN"] = response
    encoded = json.dumps(config, separators=(",", ":"))
    return graph_text[: match.start(1)] + encoded + graph_text[match.end(1) :]


def set_max_tokens(toml_text: str, max_tokens: int) -> str:
    """Set [generation].max_tokens, adding it (and the table) if absent."""
    if re.search(r"(?m)^max_tokens\s*=", toml_text):
        return re.sub(r"(?m)^max_tokens\s*=.*$", f"max_tokens = {max_tokens}", toml_text)
    if re.search(r"(?m)^\[generation\]", toml_text):
        return re.sub(r"(?m)^\[generation\]\s*$", f"[generation]\nmax_tokens = {max_tokens}", toml_text, count=1)
    return toml_text.rstrip("\n") + f"\n\n[generation]\nmax_tokens = {max_tokens}\n"


def replace_file(path: Path, text: str) -> None:
    """Write `text` to `path` by replacing the file, not rewriting it.

    A `graph.pbtxt` generated by a `--configure` run WITHOUT `--user` belongs
    to the image's own user (uid 5000) and cannot be opened for writing;
    replacing it only needs the directory to be the host user's, and gives
    the file back to that user. A crash mid-way never leaves a half-written
    file either.
    """
    tmp = path.with_name(path.name + ".npu-context.tmp")
    tmp.write_text(text)
    os.replace(tmp, path)


def self_test() -> None:
    qwen3_4b = {"num_hidden_layers": 36, "num_key_value_heads": 8,
                "num_attention_heads": 32, "hidden_size": 2560, "head_dim": 128}
    assert kv_bytes_per_token(qwen3_4b) == 147456
    coder = {"num_hidden_layers": 36, "num_key_value_heads": 2,
             "num_attention_heads": 16, "hidden_size": 2048}
    assert kv_bytes_per_token(coder) == 36864
    width = activation_width(qwen3_4b)
    prompt, response = plan_context(40960, 147456, width, 5e9)
    assert (prompt + response) % STEP == 0 and response <= prompt
    assert real_bytes(prompt + response, 147456, width) <= 5e9
    assert plan_context(2048, 1, 1, 1e12) == (1536, 512)  # capped by the model
    assert plan_context(40960, 147456, width, 0) == (512, 512)  # floor, never zero
    # Calibration points must never be underestimated (weights excluded).
    for tokens, kv, w, measured in [(7168, 147456, 92160, 5.1e9), (7168, 147456, 147456, 5.95e9),
                                    (16384, 57344, 100352, 9.19e9), (20480, 36864, 73728, 9.29e9),
                                    (32768, 147456, 92160, 31.4e9)]:
        assert real_bytes(tokens, kv, w) >= measured, (tokens, w)
    graph = "plugin_config: '{\"DEVICE_PROPERTIES\":{\"NPU\":{}}}',"
    patched = set_plugin_lengths(graph, 3072, 1024)
    assert '"MAX_PROMPT_LEN":3072' in patched and '"DEVICE_PROPERTIES"' in patched
    assert "max_tokens = 99" in set_max_tokens("[generation]\nmax_tokens = 1\n", 99)
    assert "[generation]\nmax_tokens = 7" in set_max_tokens('id = "x"\n', 7)
    print("self-test ok")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--config-dir", type=Path, default=Path.home() / ".config/npu")
    parser.add_argument("--models-dir", type=Path, default=Path.home() / "models")
    parser.add_argument("--ram-share", type=float, default=0.5,
                        help="share of total RAM granted to all NPU models together (default 0.5)")
    parser.add_argument("--apply", action="store_true", help="write graph.pbtxt and the model files")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()

    if args.self_test:
        self_test()
        return 0

    models = npu_models(args.config_dir, args.models_dir)
    if not models:
        print("npu-context: no model file points at an NPU export", file=sys.stderr)
        return 1

    weights = sum((m["export"] / "openvino_model.bin").stat().st_size for m in models)
    budget = (mem_total_bytes() * args.ram_share - weights) / len(models)
    print(f"RAM {mem_total_bytes() / 1e9:.1f} GB x {args.ram_share} - weights {weights / 1e9:.1f} GB "
          f"= {budget / 1e9:.2f} GB per NPU model ({len(models)} models)\n")
    print(f"{'model':32} {'model max':>9} {'KV/token':>9} {'prompt':>7} {'answer':>7} {'est. memory':>11}")

    writes = []
    for m in models:
        config = json.loads((m["export"] / "config.json").read_text())
        model_max = config["max_position_embeddings"]
        kv = kv_bytes_per_token(config)
        width = activation_width(config)
        prompt, response = plan_context(model_max, kv, width, budget)
        estimate = real_bytes(prompt + response, kv, width) + (m["export"] / "openvino_model.bin").stat().st_size
        print(f"{m['spec']['id']:32} {model_max:>9} {kv // 1024:>7}KB {prompt:>7} {response:>7} {estimate / 1e9:>9.1f}GB")

        # Every new content is computed before anything is written, so a
        # malformed file aborts the whole run instead of leaving half of it.
        writes.append((m["graph"], set_plugin_lengths(m["graph"].read_text(), prompt, response)))
        targets = [m["file"]]
        fallback = m["spec"].get("fallback")
        if fallback and (args.config_dir / "models" / f"{fallback}.toml").is_file():
            targets.append(args.config_dir / "models" / f"{fallback}.toml")
        writes.extend((t, set_max_tokens(t.read_text(), response)) for t in targets)

    if args.apply:
        for path, text in writes:
            replace_file(path, text)
        print("\napplied: graph.pbtxt (MAX_PROMPT_LEN, MIN_RESPONSE_LEN) and max_tokens of each model "
              "and its fallback; the next `npu backend serve` recompiles the NPU graph")
    return 0


if __name__ == "__main__":
    sys.exit(main())

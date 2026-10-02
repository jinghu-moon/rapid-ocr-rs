"""Rust 侧 `formula_eval` 的 Python/RapidDoc 参考实现（阶段 9 三方对比）。

输入是 Rust `formula_eval --manifest-output` 生成的 manifest，因此 Python 与 Rust
评测的是**同一批样本、同一顺序**，不需要在两边重复实现抽样逻辑。

用法：

```powershell
$env:RAPID_OCR_MODEL_ROOT = "<workspace>/OCR-Model"
cargo run --release --bin formula_eval -- --model <onnx> --dataset-root <Formula-TestSet> `
  --dataset latexocr --split validate `
  --manifest-output target/manifest-val.json --output target/rust-val.json --no-records

python tools/formula_reference.py --model <onnx> --dataset-root <Formula-TestSet> `
  --dataset latexocr --manifest target/manifest-val.json `
  --output target/python-val.json

cargo run --release --bin formula_eval -- --model <onnx> --dataset-root <Formula-TestSet> `
  --dataset latexocr --split validate --expect-manifest target/manifest-val.json `
  --python-reference target/python-val.json --output target/rust-val-compared.json
```

参考链路与 Rust 完全一致：RapidDoc `PPPreProcess` -> ONNX Runtime CPU -> 真实模型
metadata tokenizer -> RapidDoc `PPPostProcess`（含 `ftfy.fix_text`）。
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import sys
import types
from pathlib import Path

import numpy as np
import onnxruntime as ort
from PIL import Image
from tokenizers import Tokenizer

SUBDIRS = {
    "im2latex": "im2latex-100k",
    "latexocr": "ocr_rec_latexocr_dataset_example",
    "unimer": "UniMER-Test",
}


def load_module(path: Path, name: str):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def load_preprocessor(rapid_doc_root: Path):
    path = (
        rapid_doc_root
        / "rapid_doc/model/formula/rapid_formula_self/model_handler/pp_formulanet_plus/pre_process.py"
    )
    return load_module(path, "rd_formula_pre_process")


def load_postprocessor(rapid_doc_root: Path, character_metadata):
    base = "rapid_doc.model.formula.rapid_formula_self.model_handler.pp_formulanet_plus"
    utils_path = (
        rapid_doc_root / "rapid_doc/model/formula/rapid_formula_self/model_handler/pp_formulanet_plus/utils.py"
    )
    post_path = (
        rapid_doc_root / "rapid_doc/model/formula/rapid_formula_self/model_handler/pp_formulanet_plus/post_process.py"
    )
    utils_module = load_module(utils_path, "rd_formula_utils")
    parent_names = [
        "rapid_doc",
        "rapid_doc.model",
        "rapid_doc.model.formula",
        "rapid_doc.model.formula.rapid_formula_self",
        "rapid_doc.model.formula.rapid_formula_self.model_handler",
        "rapid_doc.model.formula.rapid_formula_self.model_handler.pp_formulanet_plus",
    ]
    for name in parent_names:
        if name not in sys.modules:
            sys.modules[name] = types.ModuleType(name)
    sys.modules[f"{base}.utils"] = utils_module
    post_module = load_module(post_path, "rd_formula_post_process")
    return post_module.PPPostProcess(character_metadata)


def decode_tokens(tokenizer: Tokenizer, tokens):
    tokens = [int(token) for token in tokens]
    eos_index = next((index for index, token in enumerate(tokens) if token == 2), None)
    if eos_index is not None:
        ids = tokens[: eos_index + 1]
        truncated = False
    else:
        ids = tokens
        truncated = True
    latex = tokenizer.decode(ids, skip_special_tokens=True)
    return latex, eos_index, truncated


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--model", required=True)
    parser.add_argument("--dataset-root", required=True)
    parser.add_argument("--dataset", required=True, choices=sorted(SUBDIRS))
    parser.add_argument("--manifest", required=True, help="Rust formula_eval 的 manifest JSON")
    parser.add_argument("--batch-size", type=int, default=8)
    parser.add_argument("--output", required=True)
    parser.add_argument("--rapid-doc-root", default="../../refer/RapidDoc-main")
    parser.add_argument("--providers", default="CPUExecutionProvider")
    args = parser.parse_args()

    manifest = json.loads(Path(args.manifest).read_text(encoding="utf-8"))
    fixture_root = Path(args.dataset_root) / SUBDIRS[args.dataset]

    session = ort.InferenceSession(args.model, providers=[args.providers])
    metadata = session.get_modelmeta().custom_metadata_map
    if "character" not in metadata:
        raise RuntimeError("model has no `character` metadata")
    character_metadata = json.loads(metadata["character"])
    tokenizer = Tokenizer.from_str(json.dumps(character_metadata["fast_tokenizer_file"]))

    rapid_doc_root = Path(args.rapid_doc_root)
    pre_module = load_preprocessor(rapid_doc_root)
    preprocess = pre_module.PPPreProcess(img_size=(384, 384))
    postprocess = load_postprocessor(rapid_doc_root, character_metadata)
    input_name = session.get_inputs()[0].name
    output_name = session.get_outputs()[0].name

    entries = manifest["entries"]
    batch_size = max(1, args.batch_size)
    records = []
    for start in range(0, len(entries), batch_size):
        chunk = entries[start : start + batch_size]
        tensors = []
        for entry in chunk:
            image_path = fixture_root / entry["relative_path"]
            image = np.array(Image.open(image_path).convert("RGB"))
            tensors.append(preprocess([image])[0])
        batch = np.concatenate(tensors, axis=0)
        output = session.run([output_name], {input_name: batch})[0]
        for row, entry in zip(output, chunk):
            tokens = [int(value) for value in row.tolist()]
            _, eos_index, truncated = decode_tokens(tokenizer, tokens)
            latex = postprocess([tokens])[0]
            records.append(
                {
                    "relative_path": entry["relative_path"],
                    "token_ids": tokens,
                    "latex": latex,
                    "eos_index": eos_index,
                    "truncated": truncated,
                }
            )
        print(f"  progress {min(start + batch_size, len(entries))}/{len(entries)}", file=sys.stderr)

    payload = {
        "reference": "RapidDoc PPPreProcess + onnxruntime + PPPostProcess",
        "model": args.model,
        "dataset": args.dataset,
        "manifest_sha256": manifest["manifest_sha256"],
        "providers": session.get_providers(),
        "records": records,
    }
    output = Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(payload, ensure_ascii=False), encoding="utf-8")
    print(
        json.dumps(
            {
                "count": len(records),
                "manifest_sha256": manifest["manifest_sha256"],
                "output": str(output),
            },
            ensure_ascii=False,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

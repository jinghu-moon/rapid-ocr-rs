import argparse
import importlib.util
import json
import sys
from pathlib import Path

import numpy as np
import onnxruntime as ort
from PIL import Image
from tokenizers import Tokenizer


def load_module(path: Path, name: str):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def load_preprocessor(rapid_doc_root: Path):
    path = rapid_doc_root / "rapid_doc/model/formula/rapid_formula_self/model_handler/pp_formulanet_plus/pre_process.py"
    return load_module(path, "rd_formula_pre_process")


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


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--model", required=True)
    parser.add_argument("--dataset-root", required=True)
    parser.add_argument("--split", default="val")
    parser.add_argument("--limit", type=int, default=0)
    parser.add_argument("--batch-size", type=int, default=8)
    parser.add_argument("--output", required=True)
    parser.add_argument("--rapid-doc-root", default="../../refer/RapidDoc-main")
    args = parser.parse_args()

    dataset_root = Path(args.dataset_root)
    list_path = dataset_root / ("val.txt" if args.split in ("val", "validate") else "train.txt")
    character_metadata = None
    session = ort.InferenceSession(args.model, providers=["CPUExecutionProvider"])
    metadata = session.get_modelmeta().custom_metadata_map
    if "character" in metadata:
        character_metadata = json.loads(metadata["character"])
    if character_metadata is None:
        raise RuntimeError("model has no character metadata")
    tokenizer = Tokenizer.from_str(json.dumps(character_metadata["fast_tokenizer_file"]))

    pre_module = load_preprocessor(Path(args.rapid_doc_root))
    preprocess = pre_module.PPPreProcess(img_size=(384, 384))
    input_name = session.get_inputs()[0].name
    output_name = session.get_outputs()[0].name

    cases = []
    for raw_line in list_path.read_text(encoding="utf-8").splitlines():
        if args.limit > 0 and len(cases) >= args.limit:
            break
        if not raw_line.strip():
            continue
        if "\t" not in raw_line:
            raise RuntimeError(f"invalid list line: {raw_line!r}")
        rel, expected = raw_line.split("\t", 1)
        cases.append((dataset_root / rel, expected))

    batch_size = max(1, args.batch_size)
    records = []
    for start in range(0, len(cases), batch_size):
        chunk = cases[start : start + batch_size]
        tensors = []
        for image_path, _ in chunk:
            image = np.array(Image.open(image_path).convert("RGB"))
            tensors.append(preprocess([image])[0])
        batch = np.concatenate(tensors, axis=0)
        output = session.run([output_name], {input_name: batch})[0]
        for row, (image_path, expected) in zip(output, chunk):
            tokens = [int(value) for value in row.tolist()]
            latex, eos_index, truncated = decode_tokens(tokenizer, tokens)
            records.append(
                {
                    "image": str(image_path),
                    "expected": expected,
                    "token_ids": tokens,
                    "latex": latex,
                    "eos_index": eos_index,
                    "truncated": truncated,
                }
            )

    Path(args.output).write_text(json.dumps({"records": records}, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps({"count": len(records), "output": args.output}, ensure_ascii=False))


if __name__ == "__main__":
    main()

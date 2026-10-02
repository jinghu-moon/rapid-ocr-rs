"""重新生成 `tests/fixtures/formula-onnx/*.onnx` 契约 fixture。

这些 fixture 随仓库提交，必须能在干净 clone / CI 中复现
`src/formula/model_info.rs`、`src/formula/session.rs`、`src/formula/recognizer.rs`
的契约测试；脚本不依赖开发机绝对路径。

用法：

```powershell
python tools/build_formula_onnx_fixtures.py
# 只重建 formula_recognizer_ok / formula_recognizer_bad_token 需要真实 tokenizer metadata：
$env:RAPID_OCR_MODEL_ROOT = "<workspace>/OCR-Model"
python tools/build_formula_onnx_fixtures.py
```

- 10 个 fixture 完全由本脚本确定性生成，不需要任何外部模型。
- `formula_recognizer_ok.onnx` / `formula_recognizer_bad_token.onnx` 内嵌真实模型的
  `character` metadata（含真实 `fast_tokenizer_file`），必须能从
  `RAPID_OCR_FORMULA_MODEL` 或 `RAPID_OCR_MODEL_ROOT` 指向的模型读取；
  缺失时脚本跳过这两个文件并返回非零退出码，不会写出伪造 tokenizer。
"""

from __future__ import annotations

import argparse
import json
import os
import sys
from pathlib import Path

import numpy as np
import onnx
from onnx import TensorProto, helper, numpy_helper

FIXTURE_DIR = Path(__file__).resolve().parent.parent / "tests" / "fixtures" / "formula-onnx"
FORMULA_MODEL_RELATIVE = "Formula-Recognition-Models/onnx/pp_formulanet_plus_m.onnx"

VOCAB_SIZE = 50_000
SPECIAL_TOKENS = [("<s>", 0), ("<pad>", 1), ("</s>", 2), ("<unk>", 3)]


def synthetic_character_metadata() -> str:
    """确定性合成 `character` metadata（50000 词表 + BOS/PAD/EOS/UNK = 0/1/2/3）。"""
    vocab = {f"tok{i}": i for i in range(VOCAB_SIZE - len(SPECIAL_TOKENS))}
    added_tokens = []
    for content, token_id in SPECIAL_TOKENS:
        vocab[content] = token_id
        added_tokens.append(
            {
                "id": token_id,
                "content": content,
                "single_word": False,
                "lstrip": False,
                "rstrip": False,
                "normalized": False,
                "special": True,
            }
        )
    return json.dumps(
        {
            "fast_tokenizer_file": {
                "version": "1.0",
                "truncation": None,
                "padding": None,
                "added_tokens": added_tokens,
                "model": {"type": "WordLevel", "unk_token": "<unk>", "vocab": vocab},
            },
            "tokenizer_config_file": {
                "bos_token": "<s>",
                "eos_token": "</s>",
                "pad_token": "<pad>",
                "unk_token": "<unk>",
                "model_max_length": 768,
            },
        },
        separators=(",", ":"),
        ensure_ascii=False,
    )


def find_real_model() -> Path | None:
    candidates = []
    if os.environ.get("RAPID_OCR_FORMULA_MODEL"):
        candidates.append(Path(os.environ["RAPID_OCR_FORMULA_MODEL"]))
    if os.environ.get("RAPID_OCR_MODEL_ROOT"):
        root = Path(os.environ["RAPID_OCR_MODEL_ROOT"])
        candidates.append(root)
        candidates.append(root / FORMULA_MODEL_RELATIVE)
    for candidate in candidates:
        if candidate.is_file():
            return candidate
    return None


def real_character_metadata(model_path: Path) -> str:
    model = onnx.load(str(model_path), load_external_data=False)
    for prop in model.metadata_props:
        if prop.key == "character":
            return prop.value
    raise RuntimeError(f"model has no `character` metadata: {model_path}")


def float_input(name: str = "x", dims=None) -> onnx.ValueInfoProto:
    dims = dims if dims is not None else ["Dyn", 1, 384, 384]
    return helper.make_tensor_value_info(name, TensorProto.FLOAT, dims)


def int64_input(name: str = "x", dims=None) -> onnx.ValueInfoProto:
    dims = dims if dims is not None else ["Dyn", 1, 384, 384]
    return helper.make_tensor_value_info(name, TensorProto.INT64, dims)


def cast_flatten_nodes(input_name: str = "x", output_name: str = "fetch_name_0", suffix: str = "0"):
    return [
        helper.make_node("Cast", [input_name], [f"casted{suffix}"], to=TensorProto.INT64),
        helper.make_node("Flatten", [f"casted{suffix}"], [output_name], axis=1),
    ]


def build_cast_flatten_model(
    *,
    input_type: str = "float",
    input_dims=None,
    output_dims=None,
    output_type: int = TensorProto.INT64,
    metadata: str | None = None,
    extra_input_names: tuple[str, ...] = (),
    extra_output: bool = False,
    output_name: str = "fetch_name_0",
) -> onnx.ModelProto:
    dims = input_dims if input_dims is not None else ["Dyn", 1, 384, 384]
    inputs = [float_input("x", dims) if input_type == "float" else int64_input("x", dims)]
    for name in extra_input_names:
        inputs.append(float_input(name, ["Dyn", 1, 384, 384]))

    out_dims = output_dims if output_dims is not None else ["Dyn", "Dyn"]
    outputs = [helper.make_tensor_value_info(output_name, output_type, out_dims)]

    if input_type == "float":
        nodes = cast_flatten_nodes("x", output_name, "0")
    else:
        nodes = [helper.make_node("Flatten", ["x"], [output_name], axis=1)]

    if extra_output:
        outputs.append(helper.make_tensor_value_info("out1", TensorProto.INT64, ["Dyn", "Dyn"]))
        nodes += [
            helper.make_node("Cast", ["x"], ["casted1"], to=TensorProto.INT64),
            helper.make_node("Flatten", ["casted1"], ["out1"], axis=1),
        ]

    graph = helper.make_graph(nodes, "formula_contract_fixture", inputs, outputs)
    model = helper.make_model(graph, producer_name="rapid-ocr-rs-fixtures")
    model.ir_version = 10
    model.opset_import[0].version = 18
    if metadata is not None:
        entry = model.metadata_props.add()
        entry.key = "character"
        entry.value = metadata
    return model


def build_token_row_model(token_row: list[int], metadata: str) -> onnx.ModelProto:
    """动态 batch graph：输出 `[N, len(token_row)]`，每行都是固定的 token_row。"""
    length = len(token_row)
    initializers = [
        numpy_helper.from_array(np.array([0], dtype=np.int64), "starts"),
        numpy_helper.from_array(np.array([1], dtype=np.int64), "ends"),
        numpy_helper.from_array(np.array([0], dtype=np.int64), "axes"),
        numpy_helper.from_array(np.array([length], dtype=np.int64), "len"),
        numpy_helper.from_array(np.array([token_row], dtype=np.int64), "token_row"),
    ]
    nodes = [
        helper.make_node("Shape", ["x"], ["shape"]),
        helper.make_node("Slice", ["shape", "starts", "ends", "axes"], ["batch_dim"]),
        helper.make_node("Concat", ["batch_dim", "len"], ["target_shape"], axis=0),
        helper.make_node("Expand", ["token_row", "target_shape"], ["fetch_name_0"]),
    ]
    graph = helper.make_graph(
        nodes,
        "formula_token_row_fixture",
        [float_input("x", ["N", 1, 384, 384])],
        [helper.make_tensor_value_info("fetch_name_0", TensorProto.INT64, ["N", length])],
        initializer=initializers,
    )
    model = helper.make_model(graph, producer_name="rapid-ocr-rs-fixtures")
    model.ir_version = 10
    model.opset_import[0].version = 18
    entry = model.metadata_props.add()
    entry.key = "character"
    entry.value = metadata
    return model


def build_rank1_output_model() -> onnx.ModelProto:
    initializers = [numpy_helper.from_array(np.array([-1], dtype=np.int64), "shape_0")]
    nodes = [
        helper.make_node("Cast", ["x"], ["casted0"], to=TensorProto.INT64),
        helper.make_node("Reshape", ["casted0", "shape_0"], ["fetch_name_0"]),
    ]
    graph = helper.make_graph(
        nodes,
        "formula_output_rank1_fixture",
        [float_input()],
        [helper.make_tensor_value_info("fetch_name_0", TensorProto.INT64, ["Dyn"])],
        initializer=initializers,
    )
    model = helper.make_model(graph, producer_name="rapid-ocr-rs-fixtures")
    model.ir_version = 10
    model.opset_import[0].version = 18
    return model


def build_float_output_model() -> onnx.ModelProto:
    graph = helper.make_graph(
        [helper.make_node("Flatten", ["x"], ["fetch_name_0"], axis=1)],
        "formula_output_f32_fixture",
        [float_input()],
        [helper.make_tensor_value_info("fetch_name_0", TensorProto.FLOAT, ["Dyn", "Dyn"])],
    )
    model = helper.make_model(graph, producer_name="rapid-ocr-rs-fixtures")
    model.ir_version = 10
    model.opset_import[0].version = 18
    return model


def build_bad_metadata_model() -> onnx.ModelProto:
    model = build_cast_flatten_model(metadata='{"fast_tokenizer_file": broken')
    return model


def fixture_models(real_metadata: str | None) -> dict[str, onnx.ModelProto]:
    synthetic = synthetic_character_metadata()
    models: dict[str, onnx.ModelProto] = {
        "formula_ok.onnx": build_cast_flatten_model(metadata=synthetic),
        "formula_no_metadata.onnx": build_cast_flatten_model(),
        "formula_bad_metadata.onnx": build_bad_metadata_model(),
        "formula_multi_input.onnx": build_cast_flatten_model(
            metadata=synthetic, extra_input_names=("x1",)
        ),
        "formula_multi_output.onnx": build_cast_flatten_model(metadata=synthetic, extra_output=True),
        "formula_input_int64.onnx": build_cast_flatten_model(input_type="int64"),
        "formula_input_rank3.onnx": build_cast_flatten_model(input_dims=["Dyn", 384, 384]),
        "formula_input_512.onnx": build_cast_flatten_model(input_dims=["Dyn", 1, 512, 512]),
        "formula_output_f32.onnx": build_float_output_model(),
        "formula_output_rank1.onnx": build_rank1_output_model(),
    }
    if real_metadata is not None:
        models["formula_recognizer_ok.onnx"] = build_token_row_model(
            [0, 82, 1769, 2], real_metadata
        )
        models["formula_recognizer_bad_token.onnx"] = build_token_row_model(
            [0, 999999, 2], real_metadata
        )
    return models


def check(folder: Path, models: dict[str, onnx.ModelProto]) -> None:
    import onnxruntime as ort

    ort.set_default_logger_severity(3)
    runnable = {"formula_ok", "formula_recognizer_ok", "formula_recognizer_bad_token"}
    for name, model in models.items():
        onnx.checker.check_model(model)
        path = folder / name
        onnx.save(model, str(path))
        session = ort.InferenceSession(str(path), providers=["CPUExecutionProvider"])
        stem = Path(name).stem
        batch = 2 if stem.startswith("formula_recognizer") else 1
        feed = {session.get_inputs()[0].name: np.zeros((batch, 1, 384, 384), dtype=np.float32)}
        try:
            outputs = session.run(None, feed)
        except Exception as error:  # noqa: BLE001 - fixtures intentionally include bad types
            if stem in runnable:
                raise
            print(f"  {name}: load ok, run rejected as designed ({type(error).__name__})")
            continue
        shape = outputs[0].shape
        dtype = outputs[0].dtype
        print(f"  {name}: run ok, output shape={shape} dtype={dtype}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--only-deterministic",
        action="store_true",
        help="跳过需要真实模型 metadata 的 formula_recognizer_* fixture",
    )
    args = parser.parse_args()

    real_metadata = None
    if not args.only_deterministic:
        model_path = find_real_model()
        if model_path is None:
            print(
                "error: 需要真实模型读取 `character` metadata；请设置 "
                "RAPID_OCR_MODEL_ROOT 或 RAPID_OCR_FORMULA_MODEL，"
                "或使用 --only-deterministic 只重建确定性 fixture。",
                file=sys.stderr,
            )
            return 2
        real_metadata = real_character_metadata(model_path)
        print(f"real metadata source: {model_path}")

    FIXTURE_DIR.mkdir(parents=True, exist_ok=True)
    models = fixture_models(real_metadata)
    check(FIXTURE_DIR, models)
    print(f"wrote {len(models)} fixtures into {FIXTURE_DIR}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

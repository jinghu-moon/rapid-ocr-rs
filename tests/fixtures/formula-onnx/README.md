# formula-onnx 测试 fixture

本目录的 `*.onnx` 为 KB 级模型契约探针测试件，被 `.gitignore` 排除（`*.onnx`），不随仓库提交。
缺失时 `src/formula/model_info.rs` 的 fixture 测试会失败。

## 重新生成

依赖 Python `onnx`（本机 1.17.0）：

```powershell
python - <<'PY'
import io, os, json, shutil
import onnx
from onnx import helper, TensorProto, numpy_helper
import numpy as np

outdir = r"tests\fixtures\formula-onnx"
# ... 生成 10 个 fixture 的完整脚本见阶段 3 提交说明
# （formula_ok / no_metadata / bad_metadata / multi_input / multi_output /
#   input_int64 / input_rank3 / output_f32 / output_rank1 / input_512）
PY
```

要点：

- `formula_ok.onnx`：`x float[Dyn,1,384,384]` -> `Cast int64` -> `Flatten(axis=1)` -> `fetch_name_0 int64[Dyn,Dyn]`，附规范 `character` metadata（vocab 50,000）
- `formula_recognizer_ok.onnx`：动态 batch graph，输出每行 `[0,82,1769,2]`；附真实 `fast_tokenizer_file` metadata，用于 `FormulaRecognizer` happy path / batch 顺序测试
- `formula_recognizer_bad_token.onnx`：输出每行 `[0,999999,2]`，用于 out-of-vocab 解码失败测试
- 负向 fixture 逐一违反：输入 rank/dtype/空间维、输出 rank/dtype、多输入/多输出、缺少或损坏 `character` metadata
- `formula_bad_metadata.onnx` 的 metadata 必须是**非 JSON 字符串**（如 `{"fast_tokenizer_file": broken`），不能是 `json.dumps(str)`

## 验证

使用 `onnxruntime`（1.20.1）确认全部 10 个 fixture 可加载、可运行，图形输出 shape 与声明一致。

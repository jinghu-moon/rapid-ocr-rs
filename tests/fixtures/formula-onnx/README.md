# formula-onnx 测试 fixture

本目录的 `*.onnx` 为 KB 级模型契约探针测试件，**随仓库提交**：`.gitignore` 中的
`*.onnx` 规则被 `!tests/fixtures/**/*.onnx` 例外放行，因此干净 clone / CI 可以直接
运行 `src/formula/model_info.rs`、`src/formula/session.rs`、`src/formula/recognizer.rs`
的契约测试，不需要本机外部模型。

## 文件

| fixture | 作用 |
| --- | --- |
| `formula_ok.onnx` | 正向契约：`x float[Dyn,1,384,384]` -> `fetch_name_0 int64[Dyn,Dyn]`，附规范 `character` metadata |
| `formula_no_metadata.onnx` | 缺少 `character` metadata |
| `formula_bad_metadata.onnx` | `character` metadata 为非 JSON 字符串 |
| `formula_multi_input.onnx` / `formula_multi_output.onnx` | 违反单输入/单输出 |
| `formula_input_int64.onnx` / `formula_input_rank3.onnx` / `formula_input_512.onnx` | 违反输入 dtype/rank/空间维 |
| `formula_output_f32.onnx` / `formula_output_rank1.onnx` | 违反输出 dtype/rank |
| `formula_recognizer_ok.onnx` | 端到端 happy path：动态 batch graph，每行输出 `[0,82,1769,2]`，附真实 `fast_tokenizer_file` metadata |
| `formula_recognizer_bad_token.onnx` | 每行输出 `[0,999999,2]`，用于 out-of-vocabulary 解码失败路径 |

## 重建

依赖 Python `onnx`（验证环境 1.17.0）与 `onnxruntime`（1.20.1）：

```powershell
python tools/build_formula_onnx_fixtures.py
```

要点：

- `formula_bad_metadata.onnx` 的 metadata 必须是**非 JSON 字符串**（如 `{"fast_tokenizer_file": broken`），不能是 `json.dumps(str)`。
- `formula_recognizer_ok.onnx` 必须使用从真实模型 `character` metadata 提取的 `fast_tokenizer_file`，否则 tokenizer golden 不再代表生产配置。
- 负向 fixture 逐一违反：输入 rank/dtype/空间维、输出 rank/dtype、多输入/多输出、缺少或损坏 `character` metadata。

## 验证

```powershell
cargo test --lib formula::model_info
cargo test --lib formula::session
cargo test --lib formula::recognizer
```

三个命令都必须在没有任何模型环境变量的干净 clone 中通过。

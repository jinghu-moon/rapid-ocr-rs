# formula-tokenizer fixtures

- `fast_tokenizer.json`：从真实 `pp_formulanet_plus_m.onnx` 的 `character` metadata 中提取的
  `fast_tokenizer_file`，已固定为 version 1.0。
- `cases.json`：30 个 token 序列和 Python `tokenizers==0.21.0` 的参考输出。规则与
  `FormulaTokenizer::decode_ids` 一致：第一个 EOS 截断并保留 EOS 位置，无 EOS 则标记
  `truncated=true`，解码时 `skip_special_tokens=true`。

`fast_tokenizer.json` 大约 1.4 MB，来源于本地模型 metadata，便于在无 ONNX 模型文件时独立回归
tokenizer 语义。

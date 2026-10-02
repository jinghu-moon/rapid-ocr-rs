# ocr-onnx 测试 fixture

本目录的 `*.onnx` 为普通 OCR session 契约测试件，被 `.gitignore` 排除（`*.onnx`），不随仓库提交。

## 文件

- `ocr_rec_ok.onnx`：FLOAT rank-4 输入 -> FLOAT rank-3 输出，CTC 识别正例；
- `ocr_rec_output_rank2.onnx`：输出 rank 错误；
- `ocr_rec_output_int64.onnx`：输出 dtype 错误；
- `ocr_rec_multi_input.onnx` / `ocr_rec_multi_output.onnx`：IO 数量错误；
- `ocr_cls_ok.onnx`：FLOAT rank-4 输入 -> FLOAT rank-2 输出，方向分类正例；
- `ocr_det_ok.onnx`：FLOAT rank-4 输入 -> FLOAT rank-4 输出，检测正例。

## 重新生成

使用 Python `onnx` 1.17.0。上述 fixture 由阶段 5 的临时生成脚本创建，核心是：

- 输入声明为 `FLOAT [Dyn, C, H, W]`；
- 输出使用 `Constant` 节点，rank/dtype 分别满足或故意违反各领域 session 契约；
- 每个模型通过 `onnx.checker.check_model` 后保存。

缺失时 `src/ocr/session.rs` 的 fixture 测试会失败。

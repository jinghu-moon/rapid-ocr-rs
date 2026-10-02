# formula-postprocess 测试 fixture

本目录把 “Rust 公式后处理与 RapidDoc 后处理的等价范围” 固化为可执行断言。

- `cases.json` 由 `tools/formula_ftfy_reference.py` 用**真实 ftfy**（本机 6.3.1）生成；
- `equivalent`：`formula::ftfy::fix_text` 必须与真实 `ftfy.fix_text` 逐字符一致；
- `documented_divergence`：真实 `ftfy.fix_text` 与“仅确定性步骤”不同，Rust 必须等于
  “仅确定性步骤”，且差异必须归因到 `formula::ftfy::UNIMPLEMENTED_STEPS` 中的步骤；
- `real_corpus_probe`：在 `Formula-TestSet` 全部 357,022 条标签上的 ftfy 生效次数
  （`changed_by_full_ftfy == changed_by_deterministic_only`，说明未实现的启发式步骤
  在当前语料上一次也没有生效）。

## 重新生成

```powershell
$env:RAPID_OCR_FORMULA_TEST_ROOT = "<workspace>/Formula-TestSet"   # 可选，用于语料探针
python tools/formula_ftfy_reference.py
```

## 结论边界

- **等价**：确定性步骤（ANSI 转义、C1 控制字符、拉丁连字、全半角、弯引号、换行、NFC、控制字符）完全等价；
- **不等价**：`unescape_html`、`fix_encoding`、`restore_byte_a0`、`replace_lossy_sequences`、
  `decode_inconsistent_utf8`。这些是启发式 mojibake 修复，未实现，且有枚举测试锁定。

因此 `postprocess_latex` **不得**被描述为完整 RapidDoc 后处理等价。

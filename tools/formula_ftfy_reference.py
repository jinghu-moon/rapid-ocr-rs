"""生成 `tests/fixtures/formula-postprocess/cases.json`。

该 fixture 把 “Rust 后处理与 RapidDoc 的等价范围” 变成可执行断言：

- `equivalent`：Rust 的 `formula::ftfy::fix_text` 必须与真实 `ftfy.fix_text` 完全一致；
- `documented_divergence`：真实 `ftfy.fix_text` 与 “仅确定性步骤” 的结果不同，
  Rust 必须与 “仅确定性步骤” 一致，且差异必须归因到 `formula::ftfy::UNIMPLEMENTED_STEPS`
  中的某个步骤。一旦有人实现了这些步骤，测试会失败并要求显式更新。

“仅确定性步骤” 由**真实 ftfy 函数**逐步执行得到，不是重写实现，
所以 Rust↔fixture 的比较是有效的移植验证。

用法：

```powershell
$env:RAPID_OCR_FORMULA_TEST_ROOT = "<workspace>/Formula-TestSet"   # 可选：补充真实语料探针
python tools/formula_ftfy_reference.py
```
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import sys
import unicodedata

import ftfy
from ftfy import fixes

ROOT = pathlib.Path(__file__).resolve().parent.parent
OUTPUT = ROOT / "tests" / "fixtures" / "formula-postprocess" / "cases.json"

IMPLEMENTED_STEPS = [
    "remove_terminal_escapes",
    "fix_c1_controls",
    "fix_latin_ligatures",
    "fix_character_width",
    "uncurl_quotes",
    "fix_line_breaks",
    "normalization(NFC)",
    "remove_control_chars",
]

# ftfy.TextFixerConfig 默认顺序中的未实现步骤。
UNIMPLEMENTED_STEPS = [
    "unescape_html",
    "fix_encoding",
    "restore_byte_a0",
    "replace_lossy_sequences",
    "decode_inconsistent_utf8",
]


def deterministic_only(text: str) -> str:
    """只执行已实现的确定性步骤，顺序与 ftfy 一致。"""
    text = fixes.remove_terminal_escapes(text)
    text = fixes.fix_c1_controls(text)
    text = fixes.fix_latin_ligatures(text)
    text = fixes.fix_character_width(text)
    text = fixes.uncurl_quotes(text)
    text = fixes.fix_line_breaks(text)
    text = unicodedata.normalize("NFC", text)
    text = fixes.remove_control_chars(text)
    return text


def first_diverging_step(text: str) -> str | None:
    """返回第一个改变文本的未实现步骤名；没有则返回 None。"""
    current = deterministic_only(text)
    if current == ftfy.fix_text(text):
        return None
    for name, function in (
        ("unescape_html", fixes.unescape_html),
        ("fix_encoding", fixes.fix_encoding),
        ("restore_byte_a0", lambda value: fixes.restore_byte_a0(value.encode("utf-8")).decode("utf-8", "replace")),
        ("replace_lossy_sequences", lambda value: fixes.replace_lossy_sequences(value.encode("utf-8")).decode("utf-8", "replace")),
        ("decode_inconsistent_utf8", fixes.decode_inconsistent_utf8),
    ):
        try:
            changed = function(current)
        except Exception:  # noqa: BLE001 - some fixers only accept bytes
            continue
        if changed != current:
            return name
    return "unknown"


def adversarial_cases() -> list[str]:
    cases = [
        # 已实现步骤
        "\u201chere\u2019s a test\u201d",
        "\u02bctest\u02bc",
        "fluffiest \ufb02\ufb03\ufb06",
        "\uff2c\uff2f\uff35\uff24\u3000\uff2e\uff2f\uff29\uff33\uff25\uff33",
        "\u01c4\u01c5\u01c6 \u0132\u0133 \u0149",
        "a\r\nb\rc\u2028d\u2029e\u0085f",
        "e\u0301",
        "\u001b[36;44mblue\u001b[0m",
        "a\u0000b\u000bc\u000ed",
        "\ufeffbom",
        # 未实现步骤
        "&amp;",
        "&lt;tag&gt;",
        "P&eacute;rez",
        "P&EACUTE;REZ",
        "Ã©tÃ©",
        "caf\u00c3\u00a9",
        "\u00e2\u0080\u0099",
        "\u00c3\u00a9",
        # 真实语料案例（UniMER sce.txt 第 4302 行）
        '\\mathrm { V o r g \u00e4 n g e \u201c }',
        # 干净的 LaTeX 必须保持不变
        "\\frac{a}{b}",
        "\\begin{array}{cc}a&b\\\\c&d\\end{array}",
        "x^{2}+y^{2}=z^{2}",
        "\\left( \\frac{a}{b} \\right)",
    ]
    return cases


def real_corpus_probe(dataset_root: pathlib.Path | None) -> dict | None:
    if dataset_root is None or not dataset_root.is_dir():
        return None
    files = [
        dataset_root / "im2latex-100k" / "im2latex_formulas.norm.lst",
        dataset_root / "ocr_rec_latexocr_dataset_example" / "val.txt",
        *(dataset_root / "UniMER-Test" / f"{name}.txt" for name in ("spe", "cpe", "sce", "hwe")),
    ]
    total = 0
    changed_by_full = 0
    changed_by_deterministic = 0
    samples: list[dict] = []
    for path in files:
        if not path.is_file():
            continue
        for line in path.read_text(encoding="utf-8", errors="replace").split("\n"):
            if "\t" in line:
                line = line.split("\t", 1)[1]
            if not line.strip():
                continue
            total += 1
            full = ftfy.fix_text(line)
            partial = deterministic_only(line)
            if full != line:
                changed_by_full += 1
            if partial != line:
                changed_by_deterministic += 1
            if full != partial and len(samples) < 20:
                samples.append(
                    {
                        "file": path.name,
                        "input": line,
                        "full_ftfy": full,
                        "deterministic_only": partial,
                        "step": first_diverging_step(line),
                    }
                )
    return {
        "description": "真实标签语料上的 ftfy 生效次数（证明未实现步骤的实测影响）",
        "lines": total,
        "changed_by_full_ftfy": changed_by_full,
        "changed_by_deterministic_only": changed_by_deterministic,
        "divergence_samples": samples,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--dataset-root", default=None)
    args = parser.parse_args()

    dataset_root = pathlib.Path(args.dataset_root or os.environ.get("RAPID_OCR_FORMULA_TEST_ROOT", ""))
    if not str(dataset_root) or not dataset_root.is_dir():
        dataset_root = None

    candidates = adversarial_cases()
    if dataset_root is not None:
        probe = real_corpus_probe(dataset_root)
        # 真实语料里出现的差异样本也进入 fixture。
        for sample in probe["divergence_samples"]:
            candidates.append(sample["input"])
    else:
        probe = None

    equivalent: list[dict] = []
    divergence: list[dict] = []
    for text in candidates:
        full = ftfy.fix_text(text)
        partial = deterministic_only(text)
        if full == partial:
            equivalent.append({"input": text, "expected": full})
        else:
            divergence.append(
                {
                    "input": text,
                    "rust_expected": partial,
                    "full_ftfy": full,
                    "step": first_diverging_step(text),
                }
            )

    for entry in divergence:
        if entry["step"] not in UNIMPLEMENTED_STEPS:
            print(
                f"error: divergence for {entry['input']!r} is not attributed to an "
                f"unimplemented step (got {entry['step']!r})",
                file=sys.stderr,
            )
            return 1

    payload = {
        "ftfy_version": ftfy.__version__,
        "generator": "tools/formula_ftfy_reference.py",
        "implemented_steps": IMPLEMENTED_STEPS,
        "unimplemented_steps": UNIMPLEMENTED_STEPS,
        "not_applicable_steps": ["fix_surrogates"],
        "equivalent": equivalent,
        "documented_divergence": divergence,
        "real_corpus_probe": probe,
    }
    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    OUTPUT.write_text(
        json.dumps(payload, ensure_ascii=False, indent=2), encoding="utf-8", newline="\n"
    )
    print(
        f"wrote {OUTPUT}: equivalent={len(equivalent)} "
        f"documented_divergence={len(divergence)} "
        f"real_corpus_probe={'yes' if probe else 'no'}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

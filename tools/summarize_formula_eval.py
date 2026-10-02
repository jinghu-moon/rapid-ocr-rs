"""把 `formula_eval` 的输出汇总成可粘贴进文档的表格，并导出失败样本。

用法：

```powershell
python tools/summarize_formula_eval.py --dir target/formula-eval --markdown target/formula-eval/summary.md
```

约定（阶段 12 的“公式 benchmark 报告格式和失败样本目录规范”）：

- 评测报告：`target/formula-eval/<dataset>[-<subset>][-<limit>].json`
- 抽样 manifest：`target/formula-eval/manifest-<dataset>[-<subset>][-<limit>].json`
- 失败样本目录：`target/formula-eval/failures-<dataset>[-<subset>].json`
- Python 参考：`target/formula-eval/python-<dataset>.json`

`-compared` 报告只用于展示 Rust/Python 链路一致性，不会重复计入主表。
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path


def load_reports(directory: Path) -> list[dict]:
    reports = []
    for path in sorted(directory.glob("*.json")):
        if path.name.startswith("manifest-"):
            continue
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except json.JSONDecodeError:
            continue
        if not isinstance(data, dict) or data.get("tool") != "formula_eval":
            continue
        data["_path"] = path
        reports.append(data)
    return reports


def percent(value: float) -> str:
    return f"{value * 100:.2f}%"


def write_failures(report: dict) -> Path | None:
    records = report.get("records")
    if not records:
        return None
    failures = [
        {
            "relative_path": record["relative_path"],
            "failure": record["failure"],
            "expected": record["expected"],
            "actual": record["actual"],
            "token_ids": record["token_ids"],
            "eos_index": record["eos_index"],
            "truncated": record["truncated"],
            "cer": record["cer"],
            "error": record["error"],
        }
        for record in records
        if record["failure"] not in ("none",)
    ]
    name = report["_path"].stem
    target = report["_path"].with_name(f"failures-{name}.json")
    target.write_text(
        json.dumps(
            {
                "dataset": report["dataset"],
                "split": report["split"],
                "subset": report.get("subset"),
                "manifest_sha256": report["manifest"]["manifest_sha256"],
                "failure_count": len(failures),
                "failures": failures,
            },
            ensure_ascii=False,
            indent=2,
        ),
        encoding="utf-8",
        newline="\n",
    )
    return target


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--dir", default="target/formula-eval")
    parser.add_argument("--markdown", default=None)
    args = parser.parse_args()

    directory = Path(args.dir)
    reports = [r for r in load_reports(directory) if not r["_path"].stem.endswith("-compared")]
    compared = [r for r in load_reports(directory) if r["_path"].stem.endswith("-compared")]

    lines = [
        "| 数据集 | 切分 | 样本 | 评分 | exact | normalized | mean CER | 链路失败 | 模型错误 | truncated | 吞吐(img/s) | P95 单图(ms) | 峰值内存(MB) | manifest |",
        "| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |",
    ]
    for report in reports:
        summary = report["summary"]
        peak = report["memory"]["peak_working_set_end_bytes"]
        lines.append(
            "| {dataset} | {split} | {total} | {scored} | {exact} | {norm} | {cer:.4f} | {pipe} | {model} | {trunc} | {tput:.3f} | {p95:.1f} | {peak} | `{sha}` |".format(
                dataset=report["dataset"],
                split=report["split"],
                total=summary["total"],
                scored=summary["scored"],
                exact=percent(summary["exact_match_rate"]),
                norm=percent(summary["normalized_match_rate"]),
                cer=summary["mean_cer"],
                pipe=summary["pipeline_failures"],
                model=summary["model_mismatches"],
                trunc=summary["truncated"],
                tput=report["throughput"]["images_per_second"],
                p95=report["throughput"]["per_image_ms"]["p95_ms"],
                peak=f"{peak / 1024 / 1024:.0f}" if peak else "n/a",
                sha=report["manifest"]["manifest_sha256"][:16],
            )
        )

    comparison_lines: list[str] = []
    for report in compared:
        reference = report.get("reference_comparison")
        if not reference:
            continue
        comparison_lines.append(
            "| {dataset} | {compared} | {token} | {latex} | {eos} | {trunc} | {both_wrong} | {link} |".format(
                dataset=report["dataset"],
                compared=reference["compared"],
                token=reference["eos_prefix_token_matches"],
                latex=reference["latex_matches"],
                eos=reference["eos_index_matches"],
                trunc=reference["truncated_matches"],
                both_wrong=reference["both_wrong"],
                link=len(reference["link_differences"]),
            )
        )

    failure_files = [str(path) for path in (write_failures(r) for r in reports) if path]

    output = "\n".join(lines)
    if comparison_lines:
        output += (
            "\n\n| 数据集 | 对比样本 | token(EOS 前)一致 | LaTeX 一致 | EOS index 一致 | truncated 一致 | 双方都错 | 链路差异 |\n"
            "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n"
            + "\n".join(comparison_lines)
        )
    if failure_files:
        output += "\n\n失败样本文件：\n" + "\n".join(f"- `{path}`" for path in failure_files)

    print(output)
    if args.markdown:
        Path(args.markdown).write_text(output + "\n", encoding="utf-8", newline="\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

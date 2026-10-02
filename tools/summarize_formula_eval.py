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
        # 分片报告只是中间产物：合并报告已经覆盖同一批样本，单独列出会重复计数。
        if ".shard" in path.name:
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


def compact_entry(report: dict) -> dict:
    summary = report["summary"]
    throughput = report["throughput"]
    return {
        "dataset": report["dataset"],
        "split": report["split"],
        "subset": report.get("subset"),
        "batch_size": report["batch_size"],
        "manifest_sha256": report["manifest"]["manifest_sha256"],
        "limit": report["manifest"]["limit"],
        "strategy": report["manifest"]["strategy"],
        "samples": summary["total"],
        "scored": summary["scored"],
        "exact_match_rate": summary["exact_match_rate"],
        "normalized_match_rate": summary["normalized_match_rate"],
        "mean_cer": summary["mean_cer"],
        "pipeline_failures": summary["pipeline_failures"],
        "model_mismatches": summary["model_mismatches"],
        "truncated": summary["truncated"],
        "failure_counts": summary["failure_counts"],
        "images_per_second": throughput["images_per_second"],
        "per_image_p50_ms": throughput["per_image_ms"]["p50_ms"],
        "per_image_p95_ms": throughput["per_image_ms"]["p95_ms"],
        "peak_working_set_bytes": report["memory"].get("peak_working_set_end_bytes"),
        "provider": report["provider"],
        "merged_from": report.get("merged_from"),
    }


def compact_benchmark(report: dict) -> dict:
    return {
        "provider_requested": report["provider_requested"],
        "provider_resolved": report["provider_resolved"],
        "provider_fallback_used": report["provider_fallback_used"],
        "rounds": report["rounds"],
        "warmup": report["warmup"],
        "session_create_ms": report["session_create_ms"],
        "first_inference_ms": report["first_inference_ms"],
        "warm_single": report["warm_single"],
        "batches": [
            {
                "batch": batch["batch"],
                "per_image_e2e_mean_ms": batch["per_image_e2e_ms"]["mean_ms"],
                "batch_e2e_mean_ms": batch["batch_e2e_ms"]["mean_ms"],
                "deterministic_tokens": batch["deterministic_tokens"],
            }
            for batch in report["batches"]
        ],
        "peak_working_set_bytes": report["memory"].get("peak_working_set_end_bytes"),
        "threads": report["threads"],
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--dir", default="target/formula-eval")
    parser.add_argument("--markdown", default=None)
    parser.add_argument(
        "--baseline-json",
        default=None,
        help="把精简后的结果写入随仓库提交的 baseline JSON",
    )
    args = parser.parse_args()

    directory = Path(args.dir)
    reports = [r for r in load_reports(directory) if not r["_path"].stem.endswith("-compared")]
    compared = [r for r in load_reports(directory) if r["_path"].stem.endswith("-compared")]

    if args.baseline_json:
        benchmarks = []
        for path in sorted(directory.glob("bench-*.json")):
            data = json.loads(path.read_text(encoding="utf-8"))
            if isinstance(data, dict) and data.get("tool") == "formula_bench":
                entry = compact_benchmark(data)
                entry["source"] = path.name
                benchmarks.append(entry)
        payload = {
            "tool": "tools/summarize_formula_eval.py",
            "regenerate": (
                "python tools/summarize_formula_eval.py --dir target/formula-eval "
                f"--baseline-json {args.baseline_json}"
            ),
            "note": (
                "由 formula_eval / formula_bench 的报告精简而来；原始报告不随仓库提交。"
                "分片运行的吞吐是并发下界，权威性能数据来自串行无争用的 bench-*.json。"
            ),
            "evaluations": [compact_entry(report) for report in reports],
            "reference_comparison": [
                {
                    "dataset": report["dataset"],
                    "manifest_sha256": report["manifest"]["manifest_sha256"],
                    **report["reference_comparison"],
                    "link_differences": len(
                        report["reference_comparison"].get("link_differences", [])
                    ),
                }
                for report in compared
                if report.get("reference_comparison")
            ],
            "benchmarks": benchmarks,
        }
        Path(args.baseline_json).write_text(
            json.dumps(payload, ensure_ascii=False, indent=2), encoding="utf-8", newline="\n"
        )
        print(f"wrote {args.baseline_json}")

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

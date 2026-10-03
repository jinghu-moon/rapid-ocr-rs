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

字段口径与 `src/bin/formula_bench.rs` / `src/bin/formula_eval.rs` 的实际输出字段一一对应：
benchmark 报告里的 provider 字段是 `provider_selected_ep`（历史报告里叫 `provider_resolved`，
该字段已删除）。缺字段时本工具抛出 :class:`ReportFieldError`，消息里同时给出报告、期望字段与
实际字段列表，而不是一个裸 `KeyError`。

回归测试：`python tools/test_summarize_formula_eval.py`
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path


class ReportFieldError(LookupError):
    """报告缺少本工具需要的字段（**定位错误**，不是裸 `KeyError`）。

    消息里必须同时给出“哪份报告”“期望哪个字段”“这份报告实际有哪些字段”，否则调用方只看到
    `KeyError: 'provider_resolved'`，无法判断是报告过旧、字段被重命名，还是目录点错了。
    """


def field(mapping: object, name: str, origin: str) -> object:
    """取字段；缺字段时抛出带定位信息的 :class:`ReportFieldError`。"""
    if not isinstance(mapping, dict):
        raise ReportFieldError(
            f"{origin}: expected an object containing field '{name}' but got "
            f"{type(mapping).__name__}"
        )
    try:
        return mapping[name]
    except KeyError:
        available = ", ".join(sorted(mapping)) or "<none>"
        raise ReportFieldError(
            f"{origin}: expected field '{name}' but this report does not contain it; "
            f"available fields: {available}"
        ) from None


def nested(mapping: object, name: str, origin: str) -> dict:
    """取嵌套字段，并断言它本身是一个对象；缺字段/类型不对都给出定位错误。"""
    value = field(mapping, name, origin)
    if not isinstance(value, dict):
        raise ReportFieldError(
            f"{origin}: field '{name}' must be an object but got {type(value).__name__}"
        )
    return value


def list_field(mapping: object, name: str, origin: str) -> list:
    """取数组字段，并断言它本身是一个数组；缺字段/类型不对都给出定位错误。"""
    value = field(mapping, name, origin)
    if not isinstance(value, list):
        raise ReportFieldError(
            f"{origin}: field '{name}' must be an array but got {type(value).__name__}"
        )
    return value


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
    origin = str(report["_path"])
    failures = [
        {
            "relative_path": field(record, "relative_path", origin),
            "failure": field(record, "failure", origin),
            "expected": field(record, "expected", origin),
            "actual": field(record, "actual", origin),
            "token_ids": field(record, "token_ids", origin),
            "eos_index": field(record, "eos_index", origin),
            "truncated": field(record, "truncated", origin),
            "cer": field(record, "cer", origin),
            "error": field(record, "error", origin),
        }
        for record in records
        if field(record, "failure", origin) not in ("none",)
    ]
    name = report["_path"].stem
    target = report["_path"].with_name(f"failures-{name}.json")
    target.write_text(
        json.dumps(
            {
                "dataset": report["dataset"],
                "split": report["split"],
                "subset": report.get("subset"),
                "manifest_sha256": field(
                    nested(report, "manifest", origin), "manifest_sha256", origin
                ),
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
    origin = str(report["_path"])
    summary = nested(report, "summary", origin)
    throughput = nested(report, "throughput", origin)
    manifest = nested(report, "manifest", origin)
    per_image = nested(throughput, "per_image_ms", origin)
    return {
        "dataset": report["dataset"],
        "split": report["split"],
        "subset": report.get("subset"),
        "batch_size": report["batch_size"],
        "manifest_sha256": field(manifest, "manifest_sha256", origin),
        "sample_set_sha256": manifest.get("sample_set_sha256"),
        "limit": field(manifest, "limit", origin),
        "strategy": field(manifest, "strategy", origin),
        "samples": field(summary, "total", origin),
        "scored": field(summary, "scored", origin),
        "exact_match_rate": field(summary, "exact_match_rate", origin),
        "normalized_match_rate": field(summary, "normalized_match_rate", origin),
        "mean_cer": field(summary, "mean_cer", origin),
        "pipeline_failures": field(summary, "pipeline_failures", origin),
        "model_mismatches": field(summary, "model_mismatches", origin),
        "truncated": field(summary, "truncated", origin),
        "failure_counts": nested(summary, "failure_counts", origin),
        "images_per_second": field(throughput, "images_per_second", origin),
        "per_image_p50_ms": field(per_image, "p50_ms", origin),
        "per_image_p95_ms": field(per_image, "p95_ms", origin),
        "peak_working_set_bytes": nested(report, "memory", origin).get(
            "peak_working_set_end_bytes"
        ),
        "provider": nested(report, "provider", origin),
        "merged_from": report.get("merged_from"),
    }


def compact_benchmark(report: dict) -> dict:
    """精简 `formula_bench` 报告。

    字段名对齐 `src/bin/formula_bench.rs` 的**当前**输出：provider 是
    `provider_selected_ep`（表示“交给 ORT 的 EP 链头部”，不是逐节点执行证据）。
    """
    origin = str(report.get("_path", "<formula_bench report>"))
    batches = list_field(report, "batches", origin)
    memory = nested(report, "memory", origin)
    return {
        "provider_requested": field(report, "provider_requested", origin),
        "provider_selected_ep": field(report, "provider_selected_ep", origin),
        "provider_fallback_used": field(report, "provider_fallback_used", origin),
        "rounds": field(report, "rounds", origin),
        "warmup": field(report, "warmup", origin),
        "session_create_ms": field(report, "session_create_ms", origin),
        "first_inference_ms": field(report, "first_inference_ms", origin),
        "warm_single": field(report, "warm_single", origin),
        "batches": [
            {
                "batch": field(batch, "batch", origin),
                "per_image_e2e_mean_ms": field(
                    nested(batch, "per_image_e2e_ms", origin), "mean_ms", origin
                ),
                "batch_e2e_mean_ms": field(
                    nested(batch, "batch_e2e_ms", origin), "mean_ms", origin
                ),
                "deterministic_tokens": field(batch, "deterministic_tokens", origin),
            }
            for batch in batches
        ],
        "peak_working_set_bytes": memory.get("peak_working_set_end_bytes"),
        "threads": field(report, "threads", origin),
    }


def compact_manifest(path: Path) -> dict:
    """从 `--manifest-output` 写出的 manifest 文件中提取数据指纹。

    这些文件由 `formula_eval --manifest-only` 重新生成（几秒级，不加载模型），
    因此 `content_sha256` 反映的是**当前磁盘上的图像内容**。
    """
    origin = str(path)
    manifest = json.loads(path.read_text(encoding="utf-8"))
    return {
        "dataset": field(manifest, "dataset", origin),
        "split": field(manifest, "split", origin),
        "subset": manifest.get("subset"),
        "strategy": field(manifest, "strategy", origin),
        "limit": field(manifest, "limit", origin),
        "entry_count": field(manifest, "entry_count", origin),
        "manifest_sha256": field(manifest, "manifest_sha256", origin),
        "sample_set_sha256": manifest.get("sample_set_sha256"),
        "content_sha256": manifest.get("content_sha256"),
        "source": path.name,
    }


def load_benchmarks(directory: Path) -> list[dict]:
    """读取目录下的 `formula_bench` 报告（按文件名排序）。"""
    benchmarks = []
    for path in sorted(directory.glob("bench-*.json")):
        data = json.loads(path.read_text(encoding="utf-8"))
        if isinstance(data, dict) and data.get("tool") == "formula_bench":
            data["_path"] = path
            benchmarks.append(data)
    return benchmarks


def build_baseline_payload(
    directory: Path, reports: list[dict], compared: list[dict], baseline_json: str
) -> dict:
    """构造随仓库提交的精简 baseline。

    顶层带 `schema_version` / `schema_epoch` / `historical`，让读者能一眼看出这份 JSON 用的是
    哪一版字段名；`benchmarks[]` 的 provider 字段名变化由 `provider_field_renames` 记录。
    """
    benchmarks = []
    for data in load_benchmarks(directory):
        entry = compact_benchmark(data)
        entry["source"] = data["_path"].name
        benchmarks.append(entry)
    return {
        "tool": "tools/summarize_formula_eval.py",
        "schema_version": 2,
        "schema_epoch": "provider_selected_ep",
        "provider_field_renames": {
            "provider_selected_ep": "renamed from `provider_resolved` when the formula "
            "bench/eval reports moved to the `selected_ep` wording"
        },
        "regenerate": (
            "python tools/summarize_formula_eval.py --dir target/formula-eval "
            f"--baseline-json {baseline_json}"
        ),
        "note": (
            "由 formula_eval / formula_bench 的报告精简而来；原始报告不随仓库提交。"
            "分片运行的吞吐是并发下界，权威性能数据来自串行无争用的 bench-*.json。"
            "dataset_manifests 记录每个数据集的内容摘要（content_sha256），"
            "可用 `formula_eval --manifest-only --expect-manifest <manifest>` 在几秒内"
            "确认图像文件没有被替换，而不需要重跑全量评测。"
        ),
        "evaluations": [compact_entry(report) for report in reports],
        "dataset_manifests": sorted(
            (compact_manifest(path) for path in directory.glob("manifest-*.json")),
            key=lambda entry: (entry["dataset"], entry["limit"], entry["entry_count"]),
        ),
        "reference_comparison": [
            {
                "dataset": report["dataset"],
                "manifest_sha256": field(
                    nested(report, "manifest", str(report["_path"])),
                    "manifest_sha256",
                    str(report["_path"]),
                ),
                **nested(report, "reference_comparison", str(report["_path"])),
                "link_differences": len(
                    nested(
                        report, "reference_comparison", str(report["_path"])
                    ).get("link_differences", [])
                ),
            }
            for report in compared
            if report.get("reference_comparison")
        ],
        "benchmarks": benchmarks,
    }


def render_markdown(reports: list[dict], compared: list[dict], directory: Path) -> str:
    lines = [
        "| 数据集 | 切分 | 样本 | 评分 | exact | normalized | mean CER | 链路失败 | 模型错误 | truncated | 吞吐(img/s) | P95 单图(ms) | 峰值内存(MB) | manifest |",
        "| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |",
    ]
    for report in reports:
        origin = str(report["_path"])
        summary = nested(report, "summary", origin)
        throughput = nested(report, "throughput", origin)
        peak = field(nested(report, "memory", origin), "peak_working_set_end_bytes", origin)
        lines.append(
            "| {dataset} | {split} | {total} | {scored} | {exact} | {norm} | {cer:.4f} | {pipe} | {model} | {trunc} | {tput:.3f} | {p95:.1f} | {peak} | `{sha}` |".format(
                dataset=report["dataset"],
                split=report["split"],
                total=field(summary, "total", origin),
                scored=field(summary, "scored", origin),
                exact=percent(field(summary, "exact_match_rate", origin)),
                norm=percent(field(summary, "normalized_match_rate", origin)),
                cer=field(summary, "mean_cer", origin),
                pipe=field(summary, "pipeline_failures", origin),
                model=field(summary, "model_mismatches", origin),
                trunc=field(summary, "truncated", origin),
                tput=field(throughput, "images_per_second", origin),
                p95=field(nested(throughput, "per_image_ms", origin), "p95_ms", origin),
                peak=f"{peak / 1024 / 1024:.0f}" if peak else "n/a",
                sha=field(nested(report, "manifest", origin), "manifest_sha256", origin)[:16],
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
    return output


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--dir", default="target/formula-eval")
    parser.add_argument("--markdown", default=None)
    parser.add_argument(
        "--baseline-json",
        default=None,
        help="把精简后的结果写入随仓库提交的 baseline JSON",
    )
    args = parser.parse_args(argv)

    directory = Path(args.dir)
    try:
        reports = [
            r for r in load_reports(directory) if not r["_path"].stem.endswith("-compared")
        ]
        compared = [
            r for r in load_reports(directory) if r["_path"].stem.endswith("-compared")
        ]

        if args.baseline_json:
            payload = build_baseline_payload(
                directory, reports, compared, args.baseline_json
            )
            Path(args.baseline_json).write_text(
                json.dumps(payload, ensure_ascii=False, indent=2),
                encoding="utf-8",
                newline="\n",
            )
            print(f"wrote {args.baseline_json}")

        output = render_markdown(reports, compared, directory)
    except ReportFieldError as error:
        # 定位错误必须以非零退出码上报，并且消息里点名缺失字段。
        print(f"error: {error}")
        return 2

    print(output)
    if args.markdown:
        Path(args.markdown).write_text(output + "\n", encoding="utf-8", newline="\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

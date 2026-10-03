"""`tools/summarize_formula_eval.py` 的回归测试。

运行方式（在本 crate 根目录，即 `crates/rapid-ocr-rs/`）：

```powershell
# 推荐：pytest（若已安装）
python -m pytest tools/test_summarize_formula_eval.py -q

# 不依赖 pytest：stdlib unittest 也能跑同一批用例
python tools/test_summarize_formula_eval.py
```

覆盖的回归点：

1. **当前 schema 必须能跑通**：用当前 `formula_bench` 字段名
   （`provider_selected_ep`）合成一份报告目录，`--baseline-json` 必须成功并写出精简结果；
   同时断言精简结果里的 provider 字段名就是当前 schema 的名字。
2. **缺字段必须是定位错误**：旧 schema（只有 `provider_resolved`）必须报出
   :class:`ReportFieldError`，且消息里**点名** `provider_selected_ep` 与实际可用字段列表，
   而不是一个裸 `KeyError`；CLI 还必须以退出码 2 结束。
3. **随仓库提交的 baseline 必须带 schema 标记**：它记录的是历史 schema
   （provider 字段早于 `selected_ep` 重命名），因此必须有 `schema_version` /
   `schema_epoch` / 说明，不能是“没有标注的过期 schema”。
"""

from __future__ import annotations

import importlib.util
import io
import json
import shutil
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path

TOOLS_DIR = Path(__file__).resolve().parent
CRATE_DIR = TOOLS_DIR.parent
SUMMARIZER_PATH = TOOLS_DIR / "summarize_formula_eval.py"
COMMITTED_BASELINE = CRATE_DIR / "tests" / "baseline" / "formula-evaluation-2026-10-03.json"


def load_summarizer():
    """按文件路径加载被测模块（不依赖包结构，也不会撞上旧的 `__pycache__`）。"""
    spec = importlib.util.spec_from_file_location("summarize_formula_eval_under_test", SUMMARIZER_PATH)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


summarizer = load_summarizer()


def current_schema_benchmark() -> dict:
    """一份**当前 schema** 的 `formula_bench` 报告（字段名对齐 `src/bin/formula_bench.rs`）。"""
    stats = {"samples": 5, "min_ms": 1.0, "max_ms": 2.0, "mean_ms": 1.5, "p50_ms": 1.5, "p95_ms": 1.9}
    return {
        "tool": "formula_bench",
        "provider_requested": "Cpu",
        "provider_selected_ep": "Cpu",
        "provider_fallback_used": False,
        "rounds": 2,
        "warmup": 1,
        "session_create_ms": 12.5,
        "first_inference_ms": 34.5,
        "warm_single": {"end_to_end": stats},
        "batches": [
            {
                "batch": 1,
                "per_image_e2e_ms": {"mean_ms": 40.0},
                "batch_e2e_ms": {"mean_ms": 40.0},
                "deterministic_tokens": True,
            },
            {
                "batch": 4,
                "per_image_e2e_ms": {"mean_ms": 22.0},
                "batch_e2e_ms": {"mean_ms": 88.0},
                "deterministic_tokens": True,
            },
        ],
        "memory": {"peak_working_set_end_bytes": 12345678},
        "threads": {"intra_threads": 14, "inter_threads": 1, "auto_tune_threads": True},
    }


def current_schema_evaluation() -> dict:
    """一份**当前 schema** 的 `formula_eval` 报告（足以驱动主表与失败样本导出）。"""
    return {
        "tool": "formula_eval",
        "dataset": "synthetic",
        "split": "test",
        "subset": None,
        "batch_size": 1,
        "manifest": {
            "dataset": "synthetic",
            "split": "test",
            "subset": None,
            "strategy": "all",
            "limit": None,
            "entry_count": 2,
            "manifest_sha256": "a" * 64,
            "sample_set_sha256": "b" * 64,
            "content_sha256": "c" * 64,
        },
        "summary": {
            "total": 2,
            "scored": 1,
            "exact_match_rate": 0.5,
            "normalized_match_rate": 0.5,
            "mean_cer": 0.25,
            "pipeline_failures": 0,
            "model_mismatches": 1,
            "truncated": 0,
            "failure_counts": {"none": 1, "token_mismatch": 1},
        },
        "throughput": {
            "images_per_second": 10.0,
            "per_image_ms": {"p50_ms": 95.0, "p95_ms": 120.0},
        },
        "memory": {"peak_working_set_end_bytes": 23456789},
        "records": [
            {
                "relative_path": "ok.png",
                "failure": "none",
                "expected": "x",
                "actual": "x",
                "token_ids": [1],
                "eos_index": 1,
                "truncated": False,
                "cer": 0.0,
                "error": None,
            },
            {
                "relative_path": "bad.png",
                "failure": "token_mismatch",
                "expected": "x",
                "actual": "y",
                "token_ids": [1],
                "eos_index": 1,
                "truncated": False,
                "cer": 1.0,
                "error": None,
            },
        ],
        "provider": {
            "requested": "Cpu",
            "selected_ep": "Cpu",
            "fallback_used": False,
            "intra_threads": 14,
            "inter_threads": 1,
            "auto_tune_threads": True,
            "effective_intra_threads": 14,
            "effective_inter_threads": 1,
            "physical_cpus": 14,
        },
    }


class CurrentSchemaBaselineTest(unittest.TestCase):
    """`--baseline-json` 必须能在当前 schema 上成功（旧的 `provider_resolved` 已删除）。"""

    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="summarize-formula-eval-"))
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.reports = self.tmp / "reports"
        self.reports.mkdir()
        (self.reports / "bench-cpu.json").write_text(
            json.dumps(current_schema_benchmark()), encoding="utf-8"
        )
        (self.reports / "synthetic.json").write_text(
            json.dumps(current_schema_evaluation()), encoding="utf-8"
        )
        (self.reports / "manifest-synthetic.json").write_text(
            json.dumps(current_schema_evaluation()["manifest"]), encoding="utf-8"
        )

    def run_summarizer(self, *extra: str) -> tuple[int, str]:
        argv = ["--dir", str(self.reports), *extra]
        stdout = io.StringIO()
        stderr = io.StringIO()
        with redirect_stdout(stdout), redirect_stderr(stderr):
            code = summarizer.main(argv)
        return code, stdout.getvalue() + stderr.getvalue()

    def test_current_schema_report_is_summarized_successfully(self) -> None:
        baseline = self.tmp / "baseline.json"
        markdown = self.tmp / "summary.md"
        code, output = self.run_summarizer(
            "--baseline-json", str(baseline), "--markdown", str(markdown)
        )
        self.assertEqual(code, 0, f"summarizer failed on a current-schema report: {output}")
        self.assertTrue(baseline.is_file(), "the baseline JSON must be written")
        self.assertTrue(markdown.is_file(), "the markdown summary must be written")

        payload = json.loads(baseline.read_text(encoding="utf-8"))
        self.assertEqual(payload["schema_version"], 2)
        self.assertEqual(payload["schema_epoch"], "provider_selected_ep")
        self.assertEqual(len(payload["benchmarks"]), 1)
        benchmark = payload["benchmarks"][0]
        self.assertEqual(benchmark["provider_selected_ep"], "Cpu")
        self.assertNotIn(
            "provider_resolved",
            benchmark,
            "the deleted field must not reappear in fresh reports",
        )
        self.assertEqual(benchmark["batches"][1]["per_image_e2e_mean_ms"], 22.0)
        self.assertEqual(len(payload["evaluations"]), 1)
        self.assertEqual(payload["evaluations"][0]["provider"]["selected_ep"], "Cpu")
        self.assertEqual(payload["evaluations"][0]["samples"], 2)

    def test_missing_field_is_a_locating_error_that_names_the_field(self) -> None:
        stale = self.reports / "bench-stale.json"
        stale.write_text(
            json.dumps(
                {
                    "tool": "formula_bench",
                    "provider_requested": "Cpu",
                    # 旧字段名：重命名之后这个键不存在了。
                    "provider_resolved": "Cpu",
                    "provider_fallback_used": False,
                    "rounds": 1,
                    "warmup": 0,
                    "session_create_ms": 1.0,
                    "first_inference_ms": 2.0,
                    "warm_single": {},
                    "batches": [],
                    "memory": {},
                    "threads": {},
                }
            ),
            encoding="utf-8",
        )
        code, output = self.run_summarizer(
            "--baseline-json", str(self.tmp / "baseline-stale.json")
        )
        self.assertEqual(code, 2, f"a stale report must fail with a locating error: {output}")
        self.assertIn("expected field 'provider_selected_ep'", output)
        self.assertIn("available fields:", output)
        self.assertIn("provider_resolved", output)
        self.assertIn("bench-stale.json", output)
        self.assertFalse(
            (self.tmp / "baseline-stale.json").exists(),
            "a failed run must not leave a partial baseline behind",
        )


class FieldHelperTest(unittest.TestCase):
    """`field()` 的行为本身：缺字段 = 定位错误，存在字段 = 原值。"""

    def test_missing_field_names_the_field_and_the_report(self) -> None:
        with self.assertRaises(summarizer.ReportFieldError) as caught:
            summarizer.field({"a": 1}, "provider_selected_ep", "bench-x.json")
        message = str(caught.exception)
        self.assertIn("bench-x.json", message)
        self.assertIn("provider_selected_ep", message)
        self.assertIn("available fields: a", message)

    def test_present_field_returns_the_value(self) -> None:
        self.assertEqual(
            summarizer.field({"provider_selected_ep": "Cuda"}, "provider_selected_ep", "x"), "Cuda"
        )

    def test_non_mapping_reports_the_expected_field(self) -> None:
        with self.assertRaises(summarizer.ReportFieldError) as caught:
            summarizer.field([], "provider_selected_ep", "bench-x.json")
        self.assertIn("provider_selected_ep", str(caught.exception))


class CommittedBaselineTest(unittest.TestCase):
    """P3：随仓库提交的 baseline 是**历史 schema**，必须带显式标记。"""

    def setUp(self) -> None:
        if not COMMITTED_BASELINE.is_file():
            self.skipTest(f"{COMMITTED_BASELINE} is not present")
        self.payload = json.loads(COMMITTED_BASELINE.read_text(encoding="utf-8"))

    def test_historical_baseline_is_labelled(self) -> None:
        self.assertEqual(self.payload.get("schema_version"), 1)
        self.assertEqual(self.payload.get("schema_epoch"), "provider_resolved")
        self.assertIs(self.payload.get("historical"), True)
        reason = self.payload.get("historical_reason", "")
        self.assertIn("provider_resolved", reason)
        self.assertIn("provider_selected_ep", reason)
        renames = self.payload.get("provider_field_renames", {})
        self.assertIn("provider_resolved", renames)
        self.assertIn("provider_selected_ep", renames["provider_resolved"])
        self.assertIn("provider_resolved", self.payload.get("note", ""))

    def test_historical_baseline_uses_the_old_benchmark_field(self) -> None:
        benchmarks = self.payload["benchmarks"]
        self.assertTrue(benchmarks, "the historical baseline must keep its benchmarks")
        for benchmark in benchmarks:
            self.assertIn("provider_resolved", benchmark)
            self.assertNotIn("provider_selected_ep", benchmark)

    def test_historical_evaluations_use_the_old_provider_object(self) -> None:
        evaluations = self.payload["evaluations"]
        self.assertTrue(evaluations, "the historical baseline must keep its evaluations")
        for evaluation in evaluations:
            provider = evaluation["provider"]
            self.assertIn("resolved", provider)
            self.assertNotIn("selected_ep", provider)


if __name__ == "__main__":
    unittest.main(verbosity=2)

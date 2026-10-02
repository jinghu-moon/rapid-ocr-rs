import argparse
import json
from pathlib import Path


def load_records(path):
    data = json.loads(Path(path).read_text(encoding="utf-8"))
    records = data["records"]
    return {Path(record["image"]).name: record for record in records}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--rust", required=True)
    parser.add_argument("--python", required=True)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()

    rust = load_records(args.rust)
    python = load_records(args.python)
    names = sorted(set(rust) | set(python))
    token_match = 0
    latex_match = 0
    eos_match = 0
    truncated_match = 0
    expected_latex_match = 0
    failures = []
    for name in names:
        r = rust.get(name)
        p = python.get(name)
        if r is None or p is None:
            failures.append({"image": name, "error": "missing on one side", "rust": r is not None, "python": p is not None})
            continue
        if r.get("error") or p.get("truncated") is None:
            failures.append({"image": name, "error": r.get("error") or "python record incomplete"})
            continue
        rust_tokens = r["token_ids"]
        python_tokens = p["token_ids"]
        if r.get("eos_index") is not None:
            rust_tokens = rust_tokens[: r["eos_index"] + 1]
            python_tokens = python_tokens[: p["eos_index"] + 1]
        token_match += rust_tokens == python_tokens
        latex_match += r["actual"] == p["latex"]
        eos_match += r["eos_index"] == p["eos_index"]
        truncated_match += r["truncated"] == p["truncated"]
        expected_latex_match += r["expected"] == r["actual"]
    count = len(names) - len(failures)
    report = {
        "count": len(names),
        "scored": count,
        "failures": failures,
        "rust_python_token_sequence_match_rate": token_match / count if count else 0.0,
        "rust_python_raw_latex_match_rate": latex_match / count if count else 0.0,
        "rust_python_eos_match_rate": eos_match / count if count else 0.0,
        "rust_python_truncated_match_rate": truncated_match / count if count else 0.0,
        "rust_expected_exact_rate": expected_latex_match / count if count else 0.0,
        "token_match_count": token_match,
        "latex_match_count": latex_match,
        "eos_match_count": eos_match,
    }
    Path(args.output).write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
    print(json.dumps(report, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()

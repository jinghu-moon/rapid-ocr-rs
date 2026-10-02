"""校验已提交的二进制 fixture 的 blob 与工作区字节完全一致。

`core.autocrlf=true` 会在 **checkout** 时把 blob 中的 0x0A 改写成 0x0D 0x0A，
从而破坏 ONNX / NumPy / PNG 等二进制文件。本脚本对 `git ls-files` 中的二进制
fixture 逐字节比较 `git cat-file blob HEAD:<path>` 与工作区文件，用于确认：
blob 未被 `git add` 破坏，工作区也未被 checkout 破坏。
"""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path

BINARY_SUFFIXES = {".onnx", ".npy", ".png", ".jpg", ".jpeg", ".webp", ".ico", ".gif"}


def git(*args: str) -> bytes:
    return subprocess.run(
        ["git", *args], check=True, capture_output=True, cwd=Path(__file__).resolve().parent.parent
    ).stdout


def main() -> int:
    root = Path(__file__).resolve().parent.parent
    files = [
        name
        for name in git("ls-files").decode("utf-8").splitlines()
        if Path(name).suffix.lower() in BINARY_SUFFIXES
    ]
    if not files:
        print("no committed binary fixtures found")
        return 1

    corrupted = []
    for name in files:
        blob = git("cat-file", "blob", f"HEAD:{name}")
        worktree = (root / name).read_bytes()
        if blob != worktree:
            corrupted.append((name, len(blob), len(worktree)))
        else:
            print(f"ok  {name} ({len(blob)} bytes)")

    if corrupted:
        print("\nMISMATCH (blob size vs worktree size):")
        for name, blob_size, worktree_size in corrupted:
            print(f"  {name}: {blob_size} vs {worktree_size}")
        return 1
    print(f"\n{len(files)} committed binary fixtures are byte-identical")
    return 0


if __name__ == "__main__":
    sys.exit(main())

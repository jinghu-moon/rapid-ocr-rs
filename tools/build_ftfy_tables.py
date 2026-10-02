"""从本机安装的 ftfy 生成 `src/formula/ftfy_tables.rs`。

生成的表必须与 ftfy 完全一致；脚本只读取 ftfy 的公开表结构，不做人工整理，
避免手抄造成静默偏差。

用法：

```powershell
python tools/build_ftfy_tables.py            # 写 src/formula/ftfy_tables.rs
python tools/build_ftfy_tables.py --check    # 只校验现有文件是否为最新
```

已覆盖的确定性步骤（见 `src/formula/ftfy.rs`）：
`uncurl_quotes`、`fix_latin_ligatures`、`fix_character_width`、`fix_c1_controls`、
`remove_control_chars`。
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

import ftfy
from ftfy import chardata, fixes

OUTPUT = Path(__file__).resolve().parent.parent / "src" / "formula" / "ftfy_tables.rs"

HEADER = '''//! `ftfy` 表的精确副本，由 `tools/build_ftfy_tables.py` 生成，请勿手工编辑。
//!
//! 生成来源：本机安装的 ftfy {version}（`ftfy.chardata` / `ftfy.fixes`）。
//! 这些表只服务于 `crate::formula::ftfy` 中确定性步骤的逐字符等价性。

/// `ftfy.chardata.LIGATURES`：拉丁连字 -> 展开后的字符串。
pub(crate) static LIGATURES: &[(char, &str)] = &[
'''

FOOTER = '''
/// `ftfy.chardata.CONTROL_CHARS`：需要删除的控制字符（`translate` 到 `None` 的键）。
pub(crate) static CONTROL_CHARS: &[char] = &[
{control_chars}];

/// C1 控制字符（U+0080..=U+009F）按 `sloppy-windows-1252` 解码后的字符。
///
/// `ftfy.fixes.fix_c1_controls` 对每个 C1 字符执行
/// `ch.encode("latin-1").decode("sloppy-windows-1252")`；这里固化其结果。
pub(crate) static C1_TO_WINDOWS_1252: &[(char, char)] = &[
{c1_map}];

/// `ftfy.fixes.SINGLE_QUOTE_RE` 的字符集合：`[\\u02bc\\u2018-\\u201b]`。
pub(crate) const CURLY_SINGLE_QUOTES: &[char] = &[
{single_quotes}];

/// `ftfy.fixes.DOUBLE_QUOTE_RE` 的字符集合：`[\\u201c-\\u201f]`。
pub(crate) const CURLY_DOUBLE_QUOTES: &[char] = &[
{double_quotes}];
'''


def rust_char(value: str) -> str:
    codepoint = ord(value)
    if value == "'":
        return r"'\''"
    if value == "\\":
        return r"'\\'"
    if 0x20 <= codepoint < 0x7F:
        return f"'{value}'"
    return f"'\\u{{{codepoint:x}}}'"


def rust_str(value: str) -> str:
    escaped = value.replace("\\", "\\\\").replace('"', '\\"')
    return f'"{escaped}"'


def build() -> str:
    lines = [HEADER.format(version=ftfy.__version__)]
    for source, target in sorted(chardata.LIGATURES.items(), key=lambda item: item[0]):
        lines.append(f"    ({rust_char(chr(source))}, {rust_str(target)}),\n")
    lines.append("];\n")

    lines.append(
        "\n/// `ftfy.chardata.WIDTH_MAP`：全角/半角形式的规范化映射（目标可能多于一个字符）。\n"
        "pub(crate) static WIDTH_MAP: &[(char, &str)] = &[\n"
    )
    for source, target in sorted(chardata.WIDTH_MAP.items(), key=lambda item: item[0]):
        lines.append(f"    ({rust_char(chr(source))}, {rust_str(target)}),\n")
    lines.append("];\n")

    control_chars = "".join(
        f"    {rust_char(chr(codepoint))},\n" for codepoint in sorted(chardata.CONTROL_CHARS)
    )
    c1_map = ""
    for codepoint in range(0x80, 0xA0):
        decoded = bytes([codepoint]).decode("sloppy-windows-1252")
        c1_map += f"    ({rust_char(chr(codepoint))}, {rust_char(decoded)}),\n"
    single = "".join(
        f"    {rust_char(chr(codepoint))},\n"
        for codepoint in sorted([0x02BC, *range(0x2018, 0x201C)])
    )
    double = "".join(
        f"    {rust_char(chr(codepoint))},\n" for codepoint in sorted(range(0x201C, 0x2020))
    )
    lines.append(
        FOOTER.format(
            control_chars=control_chars, c1_map=c1_map, single_quotes=single, double_quotes=double
        )
    )
    return "".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()

    generated = build()
    if args.check:
        current = OUTPUT.read_text(encoding="utf-8") if OUTPUT.exists() else ""
        if current != generated:
            print(f"{OUTPUT} is stale; run tools/build_ftfy_tables.py", file=sys.stderr)
            return 1
        print(f"{OUTPUT} is up to date")
        return 0

    OUTPUT.write_text(generated, encoding="utf-8", newline="\n")
    print(f"wrote {OUTPUT} ({len(generated)} bytes) from ftfy {ftfy.__version__}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

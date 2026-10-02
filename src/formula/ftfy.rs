//! `ftfy.fix_text` 的确定性步骤移植。
//!
//! RapidDoc 的公式后处理是
//! `remove_chinese_text_wrapping -> fix_latex -> ftfy.fix_text`，其中
//! `ftfy.fix_text` 使用 `TextFixerConfig` 默认值。本模块按 ftfy 的真实顺序实现
//! 其中**语义确定、可逐字符验证**的步骤，并显式记录未实现的步骤。
//!
//! # 已实现（顺序与 ftfy 一致）
//!
//! 1. `remove_terminal_escapes`：删除 ANSI 转义序列；
//! 2. `fix_c1_controls`：C1 控制字符按 `sloppy-windows-1252` 重新解释；
//! 3. `fix_latin_ligatures`：拉丁连字展开；
//! 4. `fix_character_width`：全角/半角规范化；
//! 5. `uncurl_quotes`：弯引号转直引号；
//! 6. `fix_line_breaks`：统一换行符；
//! 7. `normalization = NFC`；
//! 8. `remove_control_chars`：删除不该出现的控制字符。
//!
//! 表数据由 `tools/build_ftfy_tables.py` 从本机 ftfy 直接生成
//! （`src/formula/ftfy_tables.rs`），不做手工转录。
//!
//! # 已知未实现（显式限制）
//!
//! - `unescape_html`：需要 `html.entities.html5` 的 3501 项实体表；LaTeX 公式文本
//!   中出现 HTML 实体的概率极低，为一个大表引入逐条转录风险不划算。
//! - `fix_encoding` / `restore_byte_a0` / `replace_lossy_sequences` /
//!   `decode_inconsistent_utf8`：ftfy 的启发式 mojibake 修复，依赖 100+ 编码的
//!   字节级正则与 `sloppy` 编解码器。
//! - `fix_surrogates`：Rust `String` 不允许孤立代理项，结构上不可能出现。
//!
//! 这些步骤在 `Formula-TestSet` 全部 357,022 条标签上实测为 **0 次**生效
//! （唯一的 `ftfy.fix_text` 差异来自已实现的 `uncurl_quotes`）。`ftfy.rs` 的
//! fixture 测试把该结论固化为可执行断言：未实现步骤的差异必须逐条列出，
//! 一旦有人实现了它们，测试会失败并要求显式更新。
//!
//! 因此本模块的结论是：**确定性步骤等价，启发式 mojibake 修复不等价**，
//! 不得声称与 RapidDoc 后处理完全等价。

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;
use unicode_normalization::UnicodeNormalization;

use crate::formula::ftfy_tables::{
    C1_TO_WINDOWS_1252, CONTROL_CHARS, CURLY_DOUBLE_QUOTES, CURLY_SINGLE_QUOTES, LIGATURES,
    WIDTH_MAP,
};

/// 未实现的 ftfy 步骤，供文档、测试与评测报告引用。
pub const UNIMPLEMENTED_STEPS: &[&str] = &[
    "unescape_html",
    "fix_encoding",
    "restore_byte_a0",
    "replace_lossy_sequences",
    "decode_inconsistent_utf8",
];

/// 结构上不适用的 ftfy 步骤（Rust `String` 无法承载孤立代理项）。
pub const NOT_APPLICABLE_STEPS: &[&str] = &["fix_surrogates"];

/// `ftfy.fixes.ANSI_RE = re.compile("\033\\[((?:\\d|;)*)([a-zA-Z])")`。
static ANSI_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("\u{1b}\\[[0-9;]*[a-zA-Z]").expect("valid ANSI pattern"));

static LIGATURE_MAP: LazyLock<HashMap<char, &'static str>> =
    LazyLock::new(|| LIGATURES.iter().copied().collect());

static WIDTH_MAP_LOOKUP: LazyLock<HashMap<char, &'static str>> =
    LazyLock::new(|| WIDTH_MAP.iter().copied().collect());

static C1_MAP: LazyLock<HashMap<char, char>> =
    LazyLock::new(|| C1_TO_WINDOWS_1252.iter().copied().collect());

static CONTROL_CHAR_SET: LazyLock<Vec<bool>> = LazyLock::new(|| {
    let mut table = vec![false; 0x11000];
    for &character in CONTROL_CHARS {
        let index = character as usize;
        if index < table.len() {
            table[index] = true;
        }
    }
    table
});

fn is_control_char(character: char) -> bool {
    let table = &*CONTROL_CHAR_SET;
    let index = character as usize;
    index < table.len() && table[index]
}

fn remove_terminal_escapes(text: &str) -> String {
    ANSI_RE.replace_all(text, "").into_owned()
}

fn fix_c1_controls(text: &str) -> String {
    if !text.chars().any(|c| ('\u{80}'..='\u{9f}').contains(&c)) {
        return text.to_string();
    }
    let map = &*C1_MAP;
    text.chars()
        .map(|character| map.get(&character).copied().unwrap_or(character))
        .collect()
}

fn translate(text: &str, table: &HashMap<char, &'static str>) -> String {
    if !text.chars().any(|character| table.contains_key(&character)) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match table.get(&character) {
            Some(replacement) => out.push_str(replacement),
            None => out.push(character),
        }
    }
    out
}

fn uncurl_quotes(text: &str) -> String {
    if !text
        .chars()
        .any(|c| CURLY_SINGLE_QUOTES.contains(&c) || CURLY_DOUBLE_QUOTES.contains(&c))
    {
        return text.to_string();
    }
    text.chars()
        .map(|character| {
            if CURLY_SINGLE_QUOTES.contains(&character) {
                '\''
            } else if CURLY_DOUBLE_QUOTES.contains(&character) {
                '"'
            } else {
                character
            }
        })
        .collect()
}

fn fix_line_breaks(text: &str) -> String {
    if !text
        .chars()
        .any(|c| matches!(c, '\r' | '\u{2028}' | '\u{2029}' | '\u{85}'))
    {
        return text.to_string();
    }
    text.replace("\r\n", "\n")
        .replace(['\r', '\u{2028}', '\u{2029}', '\u{85}'], "\n")
}

fn remove_control_chars(text: &str) -> String {
    if !text.chars().any(is_control_char) {
        return text.to_string();
    }
    text.chars()
        .filter(|character| !is_control_char(*character))
        .collect()
}

/// `ftfy.fix_text` 的确定性步骤等价实现（见模块文档的已知限制）。
pub fn fix_text(text: &str) -> String {
    // 顺序与 ftfy.TextFixerConfig 默认管线一致；未实现步骤在下方注释中标注位置。
    // (1) unescape_html —— 未实现
    let text = remove_terminal_escapes(text);
    // (3-6) fix_encoding / restore_byte_a0 / replace_lossy_sequences /
    //       decode_inconsistent_utf8 —— 未实现
    let text = fix_c1_controls(&text);
    let text = translate(&text, &LIGATURE_MAP);
    let text = translate(&text, &WIDTH_MAP_LOOKUP);
    let text = uncurl_quotes(&text);
    let text = fix_line_breaks(&text);
    // fix_surrogates —— Rust String 结构上不适用
    let text: String = text.nfc().collect();
    remove_control_chars(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncurl_quotes_matches_ftfy_doctest() {
        assert_eq!(
            uncurl_quotes("\u{201c}here\u{2019}s a test\u{201d}"),
            "\"here's a test\""
        );
    }

    #[test]
    fn latin_ligatures_are_expanded() {
        assert_eq!(
            translate("\u{fb02}u\u{fb03}e\u{fb06}", &LIGATURE_MAP),
            "fluffiest"
        );
        // 'æ' 是有意保留的连字，必须原样保留。
        assert_eq!(translate("\u{e6}", &LIGATURE_MAP), "\u{e6}");
    }

    #[test]
    fn character_width_matches_ftfy_doctests() {
        assert_eq!(
            translate(
                "\u{ff2c}\u{ff2f}\u{ff35}\u{ff24}\u{3000}\u{ff2e}\u{ff2f}\u{ff29}\u{ff33}\u{ff25}\u{ff33}",
                &WIDTH_MAP_LOOKUP
            ),
            "LOUD NOISES"
        );
        assert_eq!(
            translate("\u{ff35}\u{ff80}\u{ff70}\u{ff9d}", &WIDTH_MAP_LOOKUP),
            "U\u{30bf}\u{30fc}\u{30f3}"
        );
    }

    #[test]
    fn c1_controls_are_reinterpreted_as_windows_1252() {
        assert_eq!(
            fix_c1_controls("\u{80}\u{91}\u{92}\u{9f}"),
            "\u{20ac}\u{2018}\u{2019}\u{178}"
        );
        // 未定义槽位保持不变，与 sloppy-windows-1252 一致。
        assert_eq!(fix_c1_controls("\u{81}\u{8d}\u{9d}"), "\u{81}\u{8d}\u{9d}");
    }

    #[test]
    fn terminal_escapes_are_removed() {
        assert_eq!(
            remove_terminal_escapes("\u{1b}[36;44mblue\u{1b}[0m"),
            "blue"
        );
    }

    #[test]
    fn line_breaks_are_normalized() {
        assert_eq!(
            fix_line_breaks("a\r\nb\rc\u{2028}d\u{2029}e"),
            "a\nb\nc\nd\ne"
        );
    }

    #[test]
    fn control_characters_are_removed_but_whitespace_is_kept() {
        assert_eq!(remove_control_chars("a\u{0}b\u{9}c\n"), "ab\tc\n");
        assert_eq!(remove_control_chars("a\u{feff}b\u{fffc}c"), "abc");
        // C1 控制字符不在删除集合内（它们是 mojibake 线索）。
        assert_eq!(remove_control_chars("a\u{85}b"), "a\u{85}b");
    }

    #[test]
    fn nfc_normalization_is_applied() {
        assert_eq!(fix_text("e\u{301}"), "\u{e9}");
    }

    #[test]
    fn fix_text_is_idempotent_on_clean_latex() {
        let inputs = [
            "\\frac{a}{b}",
            "\\begin{array}{cc}a&b\\\\c&d\\end{array}",
            "x^{2}+y^{2}=z^{2}",
            "\\mathrm { V o r g \" n g e }",
        ];
        for input in inputs {
            let once = fix_text(input);
            assert_eq!(once, input, "clean latex must be unchanged");
            assert_eq!(fix_text(&once), once, "fix_text must be idempotent");
        }
    }

    #[test]
    fn unimplemented_steps_are_documented() {
        assert!(UNIMPLEMENTED_STEPS.contains(&"fix_encoding"));
        assert!(UNIMPLEMENTED_STEPS.contains(&"unescape_html"));
        assert!(NOT_APPLICABLE_STEPS.contains(&"fix_surrogates"));
        // 显式限制：HTML 实体仍未解码。
        assert_eq!(fix_text("&amp;"), "&amp;");
    }

    /// 用真实 `ftfy` 生成的 fixture 锁定“确定性步骤等价 + 启发式步骤不等价”。
    mod fixture {
        use serde::Deserialize;

        use super::*;

        #[derive(Deserialize)]
        struct Case {
            input: String,
            expected: String,
        }

        #[derive(Deserialize)]
        struct Divergence {
            input: String,
            rust_expected: String,
            full_ftfy: String,
            step: String,
        }

        #[derive(Deserialize)]
        struct CorpusProbe {
            lines: u64,
            changed_by_full_ftfy: u64,
            changed_by_deterministic_only: u64,
        }

        #[derive(Deserialize)]
        struct Fixture {
            ftfy_version: String,
            implemented_steps: Vec<String>,
            unimplemented_steps: Vec<String>,
            not_applicable_steps: Vec<String>,
            equivalent: Vec<Case>,
            documented_divergence: Vec<Divergence>,
            real_corpus_probe: Option<CorpusProbe>,
        }

        fn fixture() -> Fixture {
            let path = crate::test_support::fixture_dir("formula-postprocess").join("cases.json");
            let raw = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
            serde_json::from_str(&raw).expect("fixture must be valid JSON")
        }

        /// fixture 声明的步骤集合必须与代码里的常量完全一致。
        #[test]
        fn step_lists_match_code() {
            let fixture = fixture();
            assert!(
                !fixture.ftfy_version.is_empty(),
                "fixture must record the ftfy version it was generated from"
            );
            assert_eq!(fixture.unimplemented_steps, UNIMPLEMENTED_STEPS);
            assert_eq!(fixture.not_applicable_steps, NOT_APPLICABLE_STEPS);
            for step in [
                "remove_terminal_escapes",
                "fix_c1_controls",
                "fix_latin_ligatures",
                "fix_character_width",
                "uncurl_quotes",
                "fix_line_breaks",
                "remove_control_chars",
            ] {
                assert!(
                    fixture.implemented_steps.iter().any(|it| it == step),
                    "implemented step `{step}` must be listed in the fixture"
                );
            }
        }

        /// 已实现步骤覆盖的输入：必须与真实 `ftfy.fix_text` 逐字符一致。
        #[test]
        fn implemented_steps_match_real_ftfy() {
            let fixture = fixture();
            assert!(
                !fixture.equivalent.is_empty(),
                "fixture must contain at least one equivalent case"
            );
            for case in &fixture.equivalent {
                assert_eq!(
                    fix_text(&case.input),
                    case.expected,
                    "fix_text({:?}) must equal real ftfy.fix_text",
                    case.input
                );
            }
        }

        /// 未实现步骤：Rust 必须等于“仅确定性步骤”，且差异必须被逐条列出。
        #[test]
        fn unimplemented_steps_are_enumerated_not_hidden() {
            let fixture = fixture();
            assert!(
                !fixture.documented_divergence.is_empty(),
                "the ftfy limitation must be backed by enumerated divergence cases"
            );
            for case in &fixture.documented_divergence {
                assert_eq!(
                    fix_text(&case.input),
                    case.rust_expected,
                    "fix_text({:?}) must equal the deterministic-only result",
                    case.input
                );
                assert_ne!(
                    case.full_ftfy, case.rust_expected,
                    "a documented divergence must actually diverge for {:?}",
                    case.input
                );
                assert!(
                    UNIMPLEMENTED_STEPS.contains(&case.step.as_str()),
                    "divergence for {:?} must be attributed to an unimplemented step, got `{}`",
                    case.input,
                    case.step
                );
            }
        }

        /// 真实语料探针：已实现子集覆盖了全部实测到的 ftfy 生效次数。
        #[test]
        fn deterministic_subset_covers_every_real_corpus_effect() {
            let Some(probe) = fixture().real_corpus_probe else {
                eprintln!("skipping corpus probe assertion: fixture has no real_corpus_probe");
                return;
            };
            assert!(
                probe.lines > 100_000,
                "corpus probe must cover a meaningful number of lines, got {}",
                probe.lines
            );
            assert_eq!(
                probe.changed_by_full_ftfy, probe.changed_by_deterministic_only,
                "the deterministic subset must explain every real-corpus ftfy change; if the \
                 heuristic steps now matter, they must be implemented and this fixture regenerated"
            );
        }
    }
}

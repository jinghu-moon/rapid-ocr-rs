//! RapidDoc PP-FormulaNet LaTeX 后处理。
//!
//! RapidDoc 的后处理顺序是：
//!
//! ```text
//! remove_chinese_text_wrapping
//!   -> fix_latex_left_right(fix_delimiter=False)
//!   -> fix_latex_environments
//!   -> remove_up_commands
//!   -> remove_unsupported_commands
//!   -> ftfy.fix_text
//! ```
//!
//! 本模块实现前五步（与 `fix_latex` 一致，且 RapidDoc 只以
//! `fix_delimiter=False` 调用，因此不再保留无用的 `fix_delimiter` 分支），
//! 第六步由 [`crate::formula::ftfy::fix_text`] 承担。
//!
//! **等价性边界**：`ftfy.fix_text` 只有确定性步骤实现了等价语义，
//! 启发式 mojibake 修复（`fix_encoding` 等）与 `unescape_html` 未实现，
//! 详见 [`crate::formula::ftfy`] 的模块文档。因此本函数**不等于**完整
//! RapidDoc 后处理等价，只在已评测语料上等价。
//!
//! 所有正则使用 `LazyLock` 只编译一次：`postprocess_latex` 在 10k 量级样本上
//! 逐样本调用，重复编译是明确的浪费。

use std::sync::LazyLock;

use regex::Regex;

static CHINESE_TEXT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\\text\s*\{\s*([^}]*?[\u{4e00}-\u{9fff}]+[^}]*?)\s*\}")
        .expect("valid chinese text pattern")
});
static LEFT_COMMAND_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\\left").expect("valid left pattern"));
static RIGHT_COMMAND_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\\right").expect("valid right pattern"));
static UNBALANCED_LEFT_RIGHT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\\left\.?|\\right\.?").expect("valid left/right remove pattern"));
static UP_COMMAND_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\\up([a-zA-Z]+)").expect("valid up pattern"));
static UNSUPPORTED_COMMAND_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\\(?:lefteqn|boldmath|ensuremath|centering|textsubscript|sides|textsl|textcent|emph|protect|null)",
    )
    .expect("valid unsupported command pattern")
});

pub fn postprocess_latex(raw: &str) -> String {
    let text = remove_chinese_text_wrapping(raw);
    let text = fix_latex_left_right(&text);
    let text = fix_latex_environments(&text);
    let text = remove_up_commands(&text);
    let text = remove_unsupported_commands(&text);
    crate::formula::ftfy::fix_text(&text)
}

fn remove_chinese_text_wrapping(input: &str) -> String {
    let replaced = CHINESE_TEXT_RE.replace_all(input, "$1");
    replaced.replace('"', "")
}

fn fix_latex_left_right(input: &str) -> String {
    let left_count = count_latex_command(input, &LEFT_COMMAND_RE);
    let right_count = count_latex_command(input, &RIGHT_COMMAND_RE);

    if left_count == right_count {
        fix_left_right_pairs(input)
    } else {
        UNBALANCED_LEFT_RIGHT_RE.replace_all(input, "").to_string()
    }
}

fn count_latex_command(text: &str, regex: &Regex) -> usize {
    regex
        .find_iter(text)
        .filter(|match_| {
            text[match_.end()..]
                .chars()
                .next()
                .is_none_or(|character| !character.is_ascii_alphabetic())
        })
        .count()
}

fn fix_left_right_pairs(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut brace_stack: Vec<usize> = Vec::new();
    let mut left_stack: Vec<(usize, usize, char)> = Vec::new();
    let mut adjustments: Vec<(usize, usize, usize)> = Vec::new();

    let mut i = 0usize;
    while i < chars.len() {
        if i > 0 && chars[i - 1] == '\\' {
            let mut backslashes = 0usize;
            let mut j = i;
            while j > 0 && chars[j - 1] == '\\' {
                backslashes += 1;
                j -= 1;
            }
            if backslashes % 2 == 1 {
                i += 1;
                continue;
            }
        }

        if i + 5 < chars.len()
            && chars[i] == '\\'
            && chars[i + 1] == 'l'
            && chars[i + 2] == 'e'
            && chars[i + 3] == 'f'
            && chars[i + 4] == 't'
        {
            let delimiter = chars[i + 5];
            left_stack.push((i, brace_stack.len(), delimiter));
            i += 6;
            continue;
        }

        if i + 6 < chars.len()
            && chars[i] == '\\'
            && chars[i + 1] == 'r'
            && chars[i + 2] == 'i'
            && chars[i + 3] == 'g'
            && chars[i + 4] == 'h'
            && chars[i + 5] == 't'
        {
            let _delimiter = chars[i + 6];
            if let Some((left_pos, left_depth, _)) = left_stack.pop()
                && left_depth != brace_stack.len()
            {
                let target = find_group_end(&chars, left_pos, left_depth);
                if target != usize::MAX {
                    adjustments.push((i, i + 7, target));
                }
            }
            i += 7;
            continue;
        }

        if chars[i] == '{' {
            brace_stack.push(i);
        } else if chars[i] == '}' && !brace_stack.is_empty() {
            brace_stack.pop();
        }
        i += 1;
    }

    if adjustments.is_empty() {
        return input.to_string();
    }

    let mut result = chars;
    adjustments.sort_by_key(|(start, _, _)| std::cmp::Reverse(*start));
    for (start, end, target) in adjustments {
        if start >= result.len() || end > result.len() {
            continue;
        }
        let right_part: Vec<char> = result[start..end].to_vec();
        result.drain(start..end);
        let insert_at = target.min(result.len());
        result.splice(insert_at..insert_at, right_part);
    }
    result.into_iter().collect()
}

fn find_group_end(chars: &[char], pos: usize, depth: usize) -> usize {
    let mut current_depth = depth;
    let mut i = pos;
    while i < chars.len() {
        if chars[i] == '{' && !is_escaped(chars, i) {
            current_depth += 1;
        } else if chars[i] == '}' && !is_escaped(chars, i) {
            current_depth -= 1;
            if current_depth < depth {
                return i;
            }
        }
        i += 1;
    }
    usize::MAX
}

fn is_escaped(chars: &[char], pos: usize) -> bool {
    let mut backslashes = 0usize;
    let mut j = pos;
    while j > 0 && chars[j - 1] == '\\' {
        backslashes += 1;
        j -= 1;
    }
    backslashes % 2 == 1
}

const ENVIRONMENTS: &[&str] = &[
    "array", "matrix", "pmatrix", "bmatrix", "vmatrix", "Bmatrix", "Vmatrix", "cases", "aligned",
    "gathered", "align", "align*",
];

fn fix_latex_environments(input: &str) -> String {
    let mut text = input.to_string();
    for environment in ENVIRONMENTS {
        let begin = format!("\\begin{{{environment}}}");
        let end = format!("\\end{{{environment}}}");
        let begin_count = text.matches(&begin).count();
        let end_count = text.matches(&end).count();
        if begin_count == end_count {
            continue;
        }
        if end_count > begin_count {
            let missing = end_count - begin_count;
            let format = environment_format(&text, environment);
            let command = format!("{begin}{format} ");
            text = command.repeat(missing) + &text;
        } else {
            let missing = begin_count - end_count;
            text.push_str(&(format!(" \\end{{{environment}}}")).repeat(missing));
        }
    }
    text
}

fn environment_format(text: &str, environment: &str) -> String {
    let prefix = format!("\\begin{{{environment}}}{{");
    let Some(start) = text.find(&prefix) else {
        return if environment == "array" {
            "{c}".to_string()
        } else {
            String::new()
        };
    };
    let rest = &text[start + prefix.len()..];
    let Some(end) = rest.find('}') else {
        return String::new();
    };
    format!("{{{}}}", &rest[..end])
}

fn remove_up_commands(input: &str) -> String {
    UP_COMMAND_RE
        .replace_all(input, |captures: &regex::Captures<'_>| {
            let name = captures.get(1).map(|value| value.as_str()).unwrap_or("");
            if matches!(name, "arrow" | "downarrow" | "lus" | "silon") {
                captures
                    .get(0)
                    .map(|value| value.as_str().to_string())
                    .unwrap_or_default()
            } else {
                format!("\\{name}")
            }
        })
        .to_string()
}

fn remove_unsupported_commands(input: &str) -> String {
    UNSUPPORTED_COMMAND_RE.replace_all(input, "").to_string()
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    #[test]
    fn removes_unbalanced_left_right() {
        assert_eq!(fix_latex_left_right(r"\left( x"), r"( x");
    }

    #[test]
    fn keeps_balanced_left_right_pairs() {
        assert_eq!(
            fix_latex_left_right(r"\left( x \right)"),
            r"\left( x \right)"
        );
    }

    #[test]
    fn removes_chinese_text_wrapper() {
        assert_eq!(remove_chinese_text_wrapping(r#"\text{中文}"#), "中文");
    }

    #[test]
    fn fixes_array_environment() {
        assert_eq!(
            fix_latex_environments(r"\begin{array}{c} x"),
            r"\begin{array}{c} x \end{array}"
        );
    }

    #[test]
    fn removes_up_commands_selectively() {
        assert_eq!(remove_up_commands(r"\uparrow \upalpha"), r"\uparrow \alpha");
        assert_eq!(
            remove_unsupported_commands(r"\lefteqn{x} \emph{y}"),
            r"{x} {y}"
        );
    }

    #[test]
    fn postprocess_applies_ftfy_after_fix_latex() {
        // RapidDoc 的真实案例：弯引号来自 ftfy.uncurl_quotes，且出现在 fix_latex 之后。
        assert_eq!(
            postprocess_latex("\\mathrm { V o r g \u{e4} n g e \u{201c} }"),
            "\\mathrm { V o r g \u{e4} n g e \" }"
        );
    }

    #[test]
    fn postprocess_removes_double_quotes_like_rapid_doc() {
        // remove_chinese_text_wrapping 删除所有 `"`，随后 ftfy 又把弯引号变成 `"`。
        assert_eq!(postprocess_latex("a\"b"), "ab");
        assert_eq!(postprocess_latex("a\u{201c}b"), "a\"b");
    }

    #[test]
    fn postprocess_is_idempotent_on_typical_latex() {
        let input = r"\left( \frac{a}{b} \right) \begin{array}{cc}a&b\\c&d\end{array}";
        let once = postprocess_latex(input);
        assert_eq!(postprocess_latex(&once), once);
    }

    /// 正则只编译一次：10k 次调用必须远快于重新编译 regex 的实现。
    #[test]
    fn postprocess_reuses_compiled_regexes() {
        let input = r"\left( \frac{\upalpha}{b} \right) \begin{array}";
        // 预热，把 LazyLock 初始化排除在计时之外。
        let _ = postprocess_latex(input);
        let started = Instant::now();
        for _ in 0..10_000 {
            let _ = postprocess_latex(input);
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed.as_millis() < 2_000,
            "10k postprocess calls took {elapsed:?}; regexes are probably recompiled"
        );
    }
}

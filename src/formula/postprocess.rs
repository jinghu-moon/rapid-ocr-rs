//! RapidDoc PP-FormulaNet LaTeX 后处理。
//!
//! 只实现 RapidDoc `fix_latex` 中影响最终输出的确定性子步骤；`ftfy.fix_text`
//! 的 mojibake 修复暂不引入，避免引入额外 Unicode 修正库。

use regex::Regex;

pub fn postprocess_latex(raw: &str) -> String {
    let text = remove_chinese_text_wrapping(raw);
    let text = fix_latex_left_right(&text, false);
    let text = fix_latex_environments(&text);
    let text = remove_up_commands(&text);
    remove_unsupported_commands(&text)
}

fn remove_chinese_text_wrapping(input: &str) -> String {
    let pattern = Regex::new(r"\\text\s*\{\s*([^}]*?[\u{4e00}-\u{9fff}]+[^}]*?)\s*\}")
        .expect("valid chinese text pattern");
    let replaced = pattern.replace_all(input, "$1");
    replaced.replace('"', "")
}

fn fix_latex_left_right(input: &str, fix_delimiter: bool) -> String {
    let mut text = input.to_string();
    if fix_delimiter {
        // RapidDoc's PP-FormulaNet post-process calls fix_delimiter=false.
        // Keep the branch explicit but intentionally narrow.
        let left = Regex::new(r"(\\left)(\S*)").expect("valid left pattern");
        let right = Regex::new(r"(\\right)(\S*)").expect("valid right pattern");
        text = left.replace_all(&text, "$1$2").to_string();
        text = right.replace_all(&text, "$1$2").to_string();
    }

    let left_count = count_latex_command(&text, r"\\left");
    let right_count = count_latex_command(&text, r"\\right");

    if left_count == right_count {
        fix_left_right_pairs(&text)
    } else {
        Regex::new(r"\\left\.?|\\right\.?")
            .expect("valid left/right remove pattern")
            .replace_all(&text, "")
            .to_string()
    }
}

fn count_latex_command(text: &str, pattern: &str) -> usize {
    let regex = Regex::new(pattern).expect("valid command pattern");
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
    adjustments.sort_by(|a, b| b.0.cmp(&a.0));
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
    let pattern = Regex::new(r"\\up([a-zA-Z]+)").expect("valid up pattern");
    pattern
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
    Regex::new(
        r"\\(?:lefteqn|boldmath|ensuremath|centering|textsubscript|sides|textsl|textcent|emph|protect|null)",
    )
    .expect("valid unsupported command pattern")
    .replace_all(input, "")
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_unbalanced_left_right() {
        assert_eq!(fix_latex_left_right(r"\left( x", false), r"( x");
    }

    #[test]
    fn keeps_balanced_left_right_pairs() {
        assert_eq!(
            fix_latex_left_right(r"\left( x \right)", false),
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
}

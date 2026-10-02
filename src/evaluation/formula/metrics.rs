//! Formula 识别文本指标。
//!
//! 这里只处理 LaTeX 字符串的精确/归一化匹配和字符级 CER，不依赖生产识别器。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormulaTextMetrics {
    pub exact_match: bool,
    pub normalized_match: bool,
    pub edit_distance: usize,
    pub cer: f64,
}

pub fn normalize_latex(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

pub fn edit_distance(expected: &str, actual: &str) -> usize {
    let expected: Vec<char> = expected.chars().collect();
    let actual: Vec<char> = actual.chars().collect();
    if expected.is_empty() {
        return actual.len();
    }
    if actual.is_empty() {
        return expected.len();
    }

    let mut previous: Vec<usize> = (0..=actual.len()).collect();
    let mut current = vec![0usize; actual.len() + 1];
    for (i, expected_char) in expected.iter().enumerate() {
        current[0] = i + 1;
        for (j, actual_char) in actual.iter().enumerate() {
            let insertion = current[j] + 1;
            let deletion = previous[j + 1] + 1;
            let substitution = previous[j] + usize::from(expected_char != actual_char);
            current[j + 1] = insertion.min(deletion).min(substitution);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[actual.len()]
}

pub fn character_error_rate(expected: &str, actual: &str) -> f64 {
    let expected_len = expected.chars().count();
    if expected_len == 0 {
        return if actual.is_empty() { 0.0 } else { 1.0 };
    }
    edit_distance(expected, actual) as f64 / expected_len as f64
}

pub fn evaluate_text(expected: &str, actual: &str) -> FormulaTextMetrics {
    let distance = edit_distance(expected, actual);
    FormulaTextMetrics {
        exact_match: expected == actual,
        normalized_match: normalize_latex(expected) == normalize_latex(actual),
        edit_distance: distance,
        cer: character_error_rate(expected, actual),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_distance_matches_known_examples() {
        assert_eq!(edit_distance("abc", "abc"), 0);
        assert_eq!(edit_distance("abc", "abd"), 1);
        assert_eq!(edit_distance("abc", "ab"), 1);
        assert_eq!(edit_distance("abc", "abcd"), 1);
        assert_eq!(edit_distance("", "abc"), 3);
    }

    #[test]
    fn normalized_match_ignores_whitespace() {
        assert!(evaluate_text("x + y", "x+y").normalized_match);
        assert!(!evaluate_text("x + y", "x-y").normalized_match);
    }

    #[test]
    fn cer_is_character_based() {
        let metrics = evaluate_text("abcd", "abxd");
        assert_eq!(metrics.edit_distance, 1);
        assert!((metrics.cer - 0.25).abs() < 1e-12);
    }
}

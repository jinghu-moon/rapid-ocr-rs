//! 公式识别结果序列化。
//!
//! 输出策略（三种格式各自明确，不含隐式行为）：
//!
//! - JSON：始终包含 `latex`、`eos_index`、`truncated`、`model_id`、`elapsed_ms`、
//!   `batch_size`；`token_ids` 仅在 debug 模式输出，普通文档不泄露。
//! - Markdown：使用 `$$...$$` display math。模型输出先做行尾归一化与首尾裁剪；
//!   空 LaTeX 不产生任何输出块；块内 `$$` 被转义为 `\$\$`，保证 display math 的
//!   定界符不被提前闭合；空行被折叠，避免 Markdown 在 math 内分段。
//!   `truncated=true` 时在块后追加不可见的 HTML 注释标记，既不污染渲染结果，
//!   又能被下游机械识别。
//! - HTML：LaTeX 文本与 `data-latex` 属性分别转义，并显式输出
//!   `data-truncated` / `data-eos` 供调用方判断结果是否完整。

use serde_json::{Value, json};

use crate::{
    error::Result,
    formula::recognizer::FormulaRecognition,
    output::html::{escape_attr, escape_html},
};

/// `truncated=true` 时 Markdown 追加的不可见标记。
pub const TRUNCATED_MARKER: &str = "<!-- formula truncated: no EOS token -->";

pub fn to_formula_json(result: &FormulaRecognition, include_token_ids: bool) -> Result<Value> {
    let mut object = serde_json::Map::new();
    object.insert("latex".into(), Value::String(result.latex.clone()));
    object.insert("eos_index".into(), json!(result.eos_index));
    object.insert("truncated".into(), Value::Bool(result.truncated));
    object.insert("model_id".into(), Value::String(result.model_id.clone()));
    object.insert("elapsed_ms".into(), json!(result.elapsed_ms));
    object.insert("batch_size".into(), json!(result.batch_size));
    if include_token_ids {
        object.insert("token_ids".into(), json!(result.token_ids));
    }
    Ok(Value::Object(object))
}

/// 把模型输出规范化为可以安全嵌入 display math 的单个块。
fn normalize_display_math(latex: &str) -> String {
    let normalized = latex.replace("\r\n", "\n").replace('\r', "\n");
    // 空行在 Markdown 中结束段落，也会在 LaTeX 数学模式里构成非法的段落分隔，
    // 因此这里直接丢弃空行，只保留单个换行做行分隔。
    let lines: Vec<&str> = normalized
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::trim_end)
        .collect();
    // display math 的定界符是 `$$`；LaTeX 中字面 `$` 应写作 `\$`。
    lines.join("\n").trim().replace("$$", "\\$\\$")
}

/// 把 LaTeX 渲染为 Markdown display math 块。
///
/// 空 LaTeX 返回空字符串（不产生空 math 块）；`truncated = true` 时在块后追加
/// 不可见的 HTML 注释标记，既不污染渲染结果，又能被下游机械识别。
pub fn to_markdown_block(latex: &str, truncated: bool) -> String {
    let latex = normalize_display_math(latex);
    if latex.is_empty() {
        return String::new();
    }
    let block = format!("$$\n{latex}\n$$");
    if truncated {
        format!("{block}\n\n{TRUNCATED_MARKER}")
    } else {
        block
    }
}

pub fn to_formula_markdown(result: &FormulaRecognition) -> String {
    to_markdown_block(&result.latex, result.truncated)
}

pub fn to_formula_html(result: &FormulaRecognition) -> Result<String> {
    let latex = escape_html(&result.latex);
    let attr = escape_attr(&result.latex);
    let eos = result
        .eos_index
        .map(|index| index.to_string())
        .unwrap_or_else(|| "none".to_string());
    Ok(format!(
        "<div class=\"formula\" data-latex=\"{attr}\" data-truncated=\"{}\" data-eos=\"{eos}\">\
         <span class=\"formula-latex\">{latex}</span></div>",
        result.truncated
    ))
}

/// Reorders formula results by an explicit region index list, e.g. OCR reading order.
pub fn order_formula_results(
    results: Vec<FormulaRecognition>,
    order: &[usize],
) -> Result<Vec<FormulaRecognition>> {
    if order.len() != results.len() {
        return Err(crate::error::RapidOcrError::InvalidInput(format!(
            "formula result order length mismatch: results={}, order={}",
            results.len(),
            order.len()
        )));
    }
    let mut seen = vec![false; results.len()];
    let mut ordered = Vec::with_capacity(results.len());
    for &index in order {
        if index >= results.len() || seen[index] {
            return Err(crate::error::RapidOcrError::InvalidInput(format!(
                "formula result order contains invalid index {index}"
            )));
        }
        seen[index] = true;
        ordered.push(results[index].clone());
    }
    Ok(ordered)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result_with(latex: &str, truncated: bool) -> FormulaRecognition {
        FormulaRecognition {
            latex: latex.to_string(),
            token_ids: vec![0, 82, 2],
            eos_index: if truncated { None } else { Some(2) },
            truncated,
            model_id: "test-model".to_string(),
            elapsed_ms: 1.5,
            batch_size: 1,
        }
    }

    fn result() -> FormulaRecognition {
        result_with("x < y & z", false)
    }

    #[test]
    fn json_includes_token_ids_only_when_requested() {
        let normal = to_formula_json(&result(), false).unwrap();
        assert!(normal.get("token_ids").is_none());
        assert_eq!(normal["latex"], "x < y & z");
        assert_eq!(normal["batch_size"], 1);

        let debug = to_formula_json(&result(), true).unwrap();
        assert_eq!(debug["token_ids"], serde_json::json!([0, 82, 2]));
        assert_eq!(debug["eos_index"], 2);
        assert_eq!(debug["truncated"], false);
    }

    #[test]
    fn markdown_uses_display_math_and_hides_tokens() {
        let markdown = to_formula_markdown(&result());
        assert_eq!(markdown, "$$\nx < y & z\n$$");
        assert!(!markdown.contains("82"));
    }

    #[test]
    fn markdown_escapes_inner_display_math_delimiters() {
        let markdown = to_formula_markdown(&result_with("a $$ b", false));
        assert_eq!(markdown, "$$\na \\$\\$ b\n$$");
        // 只有首尾两行可以是未转义的 `$$` 定界符。
        let delimiters = markdown.matches("$$").count();
        assert_eq!(delimiters, 2, "markdown: {markdown}");
    }

    #[test]
    fn markdown_keeps_fences_as_literal_math_content() {
        let markdown = to_formula_markdown(&result_with("```x```", false));
        assert_eq!(markdown, "$$\n```x```\n$$");
        assert!(!markdown.contains("````"), "markdown: {markdown}");
    }

    #[test]
    fn markdown_normalizes_newlines_and_drops_blank_lines() {
        let markdown = to_formula_markdown(&result_with("a\r\n b\r\n\r\n\r\nc", false));
        assert_eq!(markdown, "$$\na\n b\nc\n$$");
    }

    #[test]
    fn markdown_for_empty_latex_emits_nothing() {
        assert_eq!(to_formula_markdown(&result_with("", false)), "");
        assert_eq!(to_formula_markdown(&result_with("   \n \n", false)), "");
    }

    #[test]
    fn markdown_marks_truncated_results_with_invisible_comment() {
        let markdown = to_formula_markdown(&result_with("\\frac{1}{", true));
        assert!(markdown.starts_with("$$\n\\frac{1}{\n$$"));
        assert!(markdown.ends_with(TRUNCATED_MARKER));
        assert_eq!(
            markdown.matches(TRUNCATED_MARKER).count(),
            1,
            "markdown: {markdown}"
        );

        let complete = to_formula_markdown(&result_with("\\frac{1}{2}", false));
        assert!(!complete.contains(TRUNCATED_MARKER));
    }

    #[test]
    fn markdown_keeps_markdown_special_characters_verbatim() {
        let markdown = to_formula_markdown(&result_with("*a* _b_ [c](d) #e", false));
        assert_eq!(markdown, "$$\n*a* _b_ [c](d) #e\n$$");
    }

    #[test]
    fn html_escapes_text_and_attribute() {
        let html = to_formula_html(&result()).unwrap();
        assert!(html.contains("x &lt; y &amp; z"));
        assert!(html.contains("data-latex=\"x &lt; y &amp; z\""));
        assert!(!html.contains("<span>x"));
    }

    #[test]
    fn html_reports_truncation_and_eos_state() {
        let complete = to_formula_html(&result()).unwrap();
        assert!(complete.contains("data-truncated=\"false\""));
        assert!(complete.contains("data-eos=\"2\""));

        let truncated = to_formula_html(&result_with("\\frac{1}{", true)).unwrap();
        assert!(truncated.contains("data-truncated=\"true\""));
        assert!(truncated.contains("data-eos=\"none\""));
    }

    #[test]
    fn html_escapes_attribute_special_characters() {
        let html = to_formula_html(&result_with("a\"b'c<d>&e\nf\tg", false)).unwrap();
        assert!(
            html.contains("data-latex=\"a&quot;b&#39;c&lt;d&gt;&amp;e&#10;f&#9;g\""),
            "html: {html}"
        );
        // 属性里不能出现未转义的引号或裸换行。
        assert!(!html.contains("data-latex=\"a\"b"));
        assert!(!html.contains("&e\nf"));
    }

    #[test]
    fn html_handles_empty_latex() {
        let html = to_formula_html(&result_with("", false)).unwrap();
        assert!(html.contains("data-latex=\"\""));
        assert!(html.contains("<span class=\"formula-latex\"></span>"));
    }

    #[test]
    fn order_results_rejects_invalid_orders() {
        let results = vec![result(), result(), result()];
        let error = order_formula_results(results.clone(), &[0, 0, 2])
            .expect_err("duplicate index must fail");
        assert!(error.to_string().contains("invalid index"));

        let error =
            order_formula_results(results.clone(), &[0, 1]).expect_err("length mismatch must fail");
        assert!(error.to_string().contains("length mismatch"));

        let ordered = order_formula_results(results, &[2, 0, 1]).expect("valid order");
        assert_eq!(ordered.len(), 3);
    }
}

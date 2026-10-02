//! 公式识别结果序列化。
//!
//! JSON 可显式包含原始 token IDs；Markdown 使用 display math；HTML 对 LaTeX
//! 文本和属性分别转义。普通 Markdown 不泄露 token IDs。

use serde_json::{Value, json};

use crate::{
    error::Result,
    formula::recognizer::FormulaRecognition,
    output::html::{escape_attr, escape_html},
};

pub fn to_formula_json(result: &FormulaRecognition, include_token_ids: bool) -> Result<Value> {
    let mut object = serde_json::Map::new();
    object.insert("latex".into(), Value::String(result.latex.clone()));
    object.insert("eos_index".into(), json!(result.eos_index));
    object.insert("truncated".into(), Value::Bool(result.truncated));
    object.insert("model_id".into(), Value::String(result.model_id.clone()));
    object.insert("elapsed_ms".into(), json!(result.elapsed_ms));
    if include_token_ids {
        object.insert("token_ids".into(), json!(result.token_ids));
    }
    Ok(Value::Object(object))
}

pub fn to_formula_markdown(result: &FormulaRecognition) -> String {
    format!("$$\n{}\n$$", result.latex)
}

pub fn to_formula_html(result: &FormulaRecognition) -> Result<String> {
    let latex = escape_html(&result.latex);
    let attr = escape_attr(&result.latex);
    Ok(format!(
        "<div class=\"formula\" data-latex=\"{attr}\"><span class=\"formula-latex\">{latex}</span></div>"
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

    fn result() -> FormulaRecognition {
        FormulaRecognition {
            latex: "x < y & z".to_string(),
            token_ids: vec![0, 82, 2],
            eos_index: Some(2),
            truncated: false,
            model_id: "test-model".to_string(),
            elapsed_ms: 1.5,
        }
    }

    #[test]
    fn json_includes_token_ids_only_when_requested() {
        let normal = to_formula_json(&result(), false).unwrap();
        assert!(normal.get("token_ids").is_none());
        assert_eq!(normal["latex"], "x < y & z");

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
    fn html_escapes_text_and_attribute() {
        let html = to_formula_html(&result()).unwrap();
        assert!(html.contains("x &lt; y &amp; z"));
        assert!(html.contains("data-latex=\"x &lt; y &amp; z\""));
        assert!(!html.contains("<span>x"));
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

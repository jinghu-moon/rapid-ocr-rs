use std::{fmt::Write as _, path::Path};

use crate::{OcrJsonItem, OcrOutput, error::Result};

/// Builds an offline report shell around an image and OCR polygons.
/// `image_href` is intentionally a relative path so the report works offline.
pub fn render_report(
    title: &str,
    image_href: &str,
    width: u32,
    height: u32,
    items: &[OcrJsonItem],
    timing_summary: &str,
) -> Result<String> {
    let mut polygons = String::new();
    let mut rows = String::new();
    let mut formula_rows = String::new();
    let mut formula_count = 0usize;
    for (index, item) in items.iter().enumerate() {
        let id = format!("ocr-{index}");
        let is_formula = item.kind == "formula";
        if is_formula {
            formula_count += 1;
        }
        let (points, label_x, label_y) = item
            .box_
            .map(|points| {
                let label_x = points
                    .iter()
                    .map(|point| point[0])
                    .fold(f64::INFINITY, f64::min);
                let label_y = points
                    .iter()
                    .map(|point| point[1])
                    .fold(f64::INFINITY, f64::min);
                let points_text = points
                    .iter()
                    .map(|point| format!("{},{}", point[0], point[1]))
                    .collect::<Vec<_>>()
                    .join(" ");
                (points_text, label_x, label_y)
            })
            .unwrap_or_else(|| (String::new(), 0.0, 0.0));
        if !points.is_empty() {
            let class = if is_formula {
                "polygon formula-polygon"
            } else {
                "polygon"
            };
            writeln!(
                polygons,
                "<polygon id=\"{id}\" data-index=\"{index}\" class=\"{class}\" points=\"{}\" />\n<text class=\"polygon-label\" data-target=\"{id}\" x=\"{label_x}\" y=\"{label_y}\">{}</text>",
                escape_attr(&points),
                index + 1
            )
            .expect("writing a String cannot fail");
        }
        let row_class = if is_formula {
            "result formula-result"
        } else {
            "result"
        };
        let kind_badge = if is_formula {
            "<span class=\"kind\">公式</span>"
        } else {
            ""
        };
        writeln!(
            rows,
            "<button class=\"{row_class}\" data-target=\"{id}\">{kind_badge}<span class=\"index\">{}</span><span class=\"text\">{}</span><span class=\"score\">{:.3}</span></button>",
            index + 1,
            escape_html(&item.txt),
            item.score
        )
        .expect("writing a String cannot fail");

        if let Some(latex) = item.latex.as_ref() {
            // LaTeX 文本与 HTML 属性分别转义：属性还需要额外转义换行/制表符。
            let truncated = item.truncated.unwrap_or(false);
            writeln!(
                formula_rows,
                "<li data-target=\"{id}\"{truncated}><code class=\"latex\">{}</code><button class=\"copy\" data-latex=\"{}\">复制</button></li>",
                escape_html(latex),
                escape_attr(latex),
                truncated = if truncated { " data-truncated=\"true\"" } else { "" }
            )
            .expect("writing a String cannot fail");
        }
    }

    let formulas_section = if formula_rows.is_empty() {
        String::new()
    } else {
        format!(
            "<div class=\"formulas\"><h2>公式 ({formula_count})</h2><ol>{formula_rows}</ol></div>"
        )
    };

    Ok(format!(
        r##"<!doctype html>
<html lang="zh-CN">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>{title}</title>
  <style>
    :root {{ color-scheme: light; --ink:#18212b; --muted:#66737d; --line:#d9e0e6; --accent:#176b87; }}
    * {{ box-sizing:border-box; }}
    body {{ margin:0; background:#f3f5f7; color:var(--ink); font-family:"Segoe UI","Microsoft YaHei",sans-serif; }}
    header {{ padding:18px 24px; border-bottom:1px solid var(--line); background:#fff; }}
    h1 {{ margin:0 0 5px; font-size:20px; }}
    .meta {{ color:var(--muted); font-size:13px; }}
    main {{ display:grid; grid-template-columns:minmax(0,1fr) 330px; gap:18px; padding:18px; align-items:start; }}
    .viewer {{ min-width:0; overflow:hidden; padding:14px; border:1px solid #c9d1d8; background:#dfe4e8; }}
    .image-stage {{ position:relative; width:100%; max-width:{width}px; aspect-ratio:{width} / {height}; margin:0 auto; line-height:0; background:#fff; }}
    .image-stage img {{ display:block; width:100%; height:100%; object-fit:contain; }}
    svg {{ position:absolute; inset:0; width:100%; height:100%; overflow:visible; pointer-events:none; }}
    polygon {{ fill:#176b87; fill-opacity:.08; stroke:#176b87; stroke-width:2; vector-effect:non-scaling-stroke; pointer-events:auto; cursor:pointer; }}
    polygon:hover, polygon.active {{ fill:#f08a24; fill-opacity:.25; stroke:#d46500; stroke-width:3; }}
    .polygon-label {{ fill:#fff; stroke:#176b87; stroke-width:6; paint-order:stroke; stroke-linejoin:round; font:700 22px Consolas,monospace; pointer-events:none; dominant-baseline: hanging; }}
    .polygon-label.active {{ fill:#fff; stroke:#d46500; }}
    aside {{ position:sticky; top:18px; max-height:calc(100vh - 36px); overflow:auto; border:1px solid var(--line); background:#fff; }}
    .summary {{ padding:14px 15px; border-bottom:1px solid var(--line); color:var(--muted); font-size:13px; line-height:1.6; }}
    .results {{ display:flex; flex-direction:column; }}
    .result {{ display:grid; grid-template-columns:30px minmax(0,1fr) 48px; gap:8px; align-items:start; padding:10px 12px; border:0; border-bottom:1px solid #edf0f2; background:#fff; color:var(--ink); text-align:left; font:inherit; cursor:pointer; }}
    .result:hover, .result.active {{ background:#eef6f8; }}
    .kind {{ color:#8a2be2; font:11px "Microsoft YaHei",sans-serif; }}
    .formula-polygon {{ fill:#8a2be2; fill-opacity:.08; stroke:#8a2be2; }}
    .formula-polygon:hover, .formula-polygon.active {{ fill:#f08a24; fill-opacity:.25; stroke:#d46500; }}
    .formulas {{ padding:12px 14px; border-top:1px solid var(--line); }}
    .formulas h2 {{ margin:0 0 8px; font-size:14px; }}
    .formulas ol {{ margin:0; padding-left:18px; }}
    .formulas li {{ margin-bottom:8px; line-height:1.5; }}
    .formulas li.active {{ background:#eef6f8; }}
    .formulas code.latex {{ display:block; overflow-wrap:anywhere; font:12px Consolas,monospace; }}
    .formulas button.copy {{ margin-top:2px; border:1px solid var(--line); background:#fff; cursor:pointer; font:11px inherit; }}
    .index {{ color:var(--accent); font:12px Consolas,monospace; }}
    .text {{ overflow-wrap:anywhere; line-height:1.45; }}
    .score {{ color:var(--muted); font:12px Consolas,monospace; text-align:right; }}
    @media (max-width:900px) {{ main {{ grid-template-columns:1fr; }} aside {{ position:static; max-height:none; }} }}
  </style>
</head>
<body>
  <header><h1>{title}</h1><div class="meta">原图 {width} × {height} px · {count} 个区域 · {formula_count} 个公式 · {timing}</div></header>
  <main>
    <section class="viewer"><div class="image-stage"><img src="{image_href}" alt="OCR source"><svg viewBox="0 0 {width} {height}" aria-label="OCR polygons">{polygons}</svg></div></section>
    <aside><div class="summary">点击右侧文本或图片中的框，可检查原位坐标与识别结果是否对应。公式区域渲染为 LaTeX，不经过普通文本通道。</div><div class="results">{rows}</div>{formulas_section}</aside>
  </main>
  <script>
    const items = [...document.querySelectorAll('.result')];
    const polygons = [...document.querySelectorAll('polygon')];
    const labels = [...document.querySelectorAll('.polygon-label')];
    const formulaItems = [...document.querySelectorAll('.formulas li')];
    function select(id, source) {{
      items.forEach(item => item.classList.toggle('active', item.dataset.target === id));
      polygons.forEach(poly => poly.classList.toggle('active', poly.id === id));
      labels.forEach(label => label.classList.toggle('active', label.dataset.target === id));
      formulaItems.forEach(item => item.classList.toggle('active', item.dataset.target === id));
      if (source === 'image') {{
        document.querySelector(`.result[data-target="${{id}}"]`)?.scrollIntoView({{ behavior:'smooth', block:'nearest' }});
      }}
    }}
    items.forEach(item => item.addEventListener('click', () => select(item.dataset.target, 'result')));
    polygons.forEach(poly => poly.addEventListener('click', () => select(poly.id, 'image')));
    formulaItems.forEach(item => item.addEventListener('click', (event) => {{
      if (event.target.closest('.copy')) return;
      select(item.dataset.target, 'formula');
    }}));
    document.querySelectorAll('.formulas button.copy').forEach(button => {{
      button.addEventListener('click', async (event) => {{
        event.stopPropagation();
        try {{ await navigator.clipboard.writeText(button.dataset.latex); button.textContent = '已复制'; }}
        catch (error) {{ button.textContent = '复制失败'; }}
      }});
    }});
  </script>
</body>
</html>
"##,
        title = escape_html(title),
        image_href = escape_attr(image_href),
        width = width,
        height = height,
        count = items.len(),
        formula_count = formula_count,
        timing = escape_html(timing_summary),
        polygons = polygons,
        rows = rows,
        formulas_section = formulas_section,
    ))
}

pub fn relative_image_name(image_path: &Path) -> String {
    let stem = image_path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("source");
    let extension = image_path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("png");
    format!("{stem}-source.{extension}")
}

pub fn render_output_report(
    title: &str,
    image_href: &str,
    output: &OcrOutput,
    timing_summary: &str,
) -> Result<String> {
    let items = crate::output::json::to_output_items(output);
    render_report(
        title,
        image_href,
        output.image.original_size.width,
        output.image.original_size.height,
        &items,
        timing_summary,
    )
}

pub(crate) fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// 转义 HTML 属性值。
///
/// 属性值在 HTML 解析阶段会把换行与制表符规范化为空格，因此属性必须比文本节点
/// 多转义这些字符，否则 LaTeX 内容在往返后会发生变化。
pub(crate) fn escape_attr(value: &str) -> String {
    escape_html(value)
        .replace('\n', "&#10;")
        .replace('\r', "&#13;")
        .replace('\t', "&#9;")
}

#[cfg(test)]
mod tests {
    use super::{relative_image_name, render_report};
    use crate::OcrJsonItem;

    #[test]
    fn report_contains_original_coordinates_and_escaped_text() {
        let html = render_report(
            "测试 <报告>",
            "source.png",
            640,
            480,
            &[OcrJsonItem {
                box_: Some([[1.0, 2.0], [30.0, 2.0], [30.0, 18.0], [1.0, 18.0]]),
                txt: "<hello> & world".into(),
                score: 0.875,
                kind: "text",
                latex: None,
                eos_index: None,
                truncated: None,
            }],
            "total 12.3 ms",
        )
        .expect("report should render");
        assert!(html.contains("points=\"1,2 30,2 30,18 1,18\""));
        assert!(html.contains("class=\"polygon-label\""));
        assert!(html.contains("aspect-ratio:640 / 480"));
        assert!(html.contains("scrollIntoView"));
        assert!(html.contains("&lt;hello&gt; &amp; world"));
        assert!(!html.contains("<hello> & world"));
    }

    #[test]
    fn report_renders_formula_regions_as_escaped_latex() {
        let html = render_report(
            "公式报告",
            "source.png",
            640,
            480,
            &[
                OcrJsonItem {
                    box_: Some([[1.0, 2.0], [30.0, 2.0], [30.0, 18.0], [1.0, 18.0]]),
                    txt: "text".into(),
                    score: 0.9,
                    kind: "text",
                    latex: None,
                    eos_index: None,
                    truncated: None,
                },
                OcrJsonItem {
                    box_: Some([[1.0, 40.0], [80.0, 40.0], [80.0, 70.0], [1.0, 70.0]]),
                    txt: "a<b & c".into(),
                    score: 0.71,
                    kind: "formula",
                    latex: Some("a<b & c\nd".into()),
                    eos_index: None,
                    truncated: Some(true),
                },
            ],
            "total 12.3 ms",
        )
        .expect("report should render");

        assert!(html.contains("1 个公式"), "html: {html}");
        assert!(html.contains("formula-polygon"), "html: {html}");
        assert!(html.contains("class=\"formulas\""), "html: {html}");
        // LaTeX 文本节点与属性都必须转义，且属性里的换行被编码。
        assert!(html.contains("a&lt;b &amp; c"), "html: {html}");
        assert!(
            html.contains("data-latex=\"a&lt;b &amp; c&#10;d\""),
            "html: {html}"
        );
        assert!(html.contains("data-truncated=\"true\""), "html: {html}");
        assert!(
            !html.contains("a<b & c"),
            "raw LaTeX must never appear unescaped"
        );
    }

    #[test]
    fn image_name_preserves_extension() {
        assert_eq!(
            relative_image_name(std::path::Path::new("case 01.jpg")),
            "case 01-source.jpg"
        );
    }
}

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
    for (index, item) in items.iter().enumerate() {
        let id = format!("ocr-{index}");
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
            writeln!(
                polygons,
                "<polygon id=\"{id}\" data-index=\"{index}\" points=\"{}\" />\n<text class=\"polygon-label\" data-target=\"{id}\" x=\"{label_x}\" y=\"{label_y}\">{}</text>",
                escape_attr(&points),
                index + 1
            )
            .expect("writing a String cannot fail");
        }
        writeln!(
            rows,
            "<button class=\"result\" data-target=\"{id}\"><span class=\"index\">{}</span><span class=\"text\">{}</span><span class=\"score\">{:.3}</span></button>",
            index + 1,
            escape_html(&item.txt),
            item.score
        )
        .expect("writing a String cannot fail");
    }

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
    .index {{ color:var(--accent); font:12px Consolas,monospace; }}
    .text {{ overflow-wrap:anywhere; line-height:1.45; }}
    .score {{ color:var(--muted); font:12px Consolas,monospace; text-align:right; }}
    @media (max-width:900px) {{ main {{ grid-template-columns:1fr; }} aside {{ position:static; max-height:none; }} }}
  </style>
</head>
<body>
  <header><h1>{title}</h1><div class="meta">原图 {width} × {height} px · {count} 个文本区域 · {timing}</div></header>
  <main>
    <section class="viewer"><div class="image-stage"><img src="{image_href}" alt="OCR source"><svg viewBox="0 0 {width} {height}" aria-label="OCR polygons">{polygons}</svg></div></section>
    <aside><div class="summary">点击右侧文本或图片中的框，可检查原位坐标与识别结果是否对应。</div><div class="results">{rows}</div></aside>
  </main>
  <script>
    const items = [...document.querySelectorAll('.result')];
    const polygons = [...document.querySelectorAll('polygon')];
    const labels = [...document.querySelectorAll('.polygon-label')];
    function select(id, source) {{
      items.forEach(item => item.classList.toggle('active', item.dataset.target === id));
      polygons.forEach(poly => poly.classList.toggle('active', poly.id === id));
      labels.forEach(label => label.classList.toggle('active', label.dataset.target === id));
      if (source === 'image') {{
        document.querySelector(`.result[data-target="${{id}}"]`)?.scrollIntoView({{ behavior:'smooth', block:'nearest' }});
      }}
    }}
    items.forEach(item => item.addEventListener('click', () => select(item.dataset.target, 'result')));
    polygons.forEach(poly => poly.addEventListener('click', () => select(poly.id, 'image')));
  </script>
</body>
</html>
"##,
        title = escape_html(title),
        image_href = escape_attr(image_href),
        width = width,
        height = height,
        count = items.len(),
        timing = escape_html(timing_summary),
        polygons = polygons,
        rows = rows,
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

pub(crate) fn escape_attr(value: &str) -> String {
    escape_html(value)
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
    fn image_name_preserves_extension() {
        assert_eq!(
            relative_image_name(std::path::Path::new("case 01.jpg")),
            "case 01-source.jpg"
        );
    }
}

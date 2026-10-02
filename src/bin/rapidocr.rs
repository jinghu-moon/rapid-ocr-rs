use clap::{Parser, Subcommand};
use rapid_ocr_rs::evaluation::ocr::{
    EvaluationCase, EvaluationReport, EvaluationSummary, evaluate_case,
};
use rapid_ocr_rs::{
    ClassifierPlan, ClassifierPolicy, DetectionPolicy, EngineConfig, FormulaPolicy, ImageInput,
    OcrEngine, OcrRequest, OutputPolicy, PreprocessPolicy, RapidOcrEngine, RecognitionPolicy,
    StagePlan, TextOrder, WordOutputMode, render_output_report, to_output_items, to_output_json,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

/// 页面级公式路由的 CLI 选项。
#[derive(Debug, Clone, Default, clap::Args)]
struct FormulaArgs {
    /// 公式识别模型（`pp_formulanet_plus_m.onnx`）；给出即启用公式路由。
    #[arg(long = "formula-model", value_name = "ONNX")]
    model: Option<PathBuf>,
    /// 页面公式检测模型（`pix2text-mfd-1.5.onnx`）；不给出时只处理显式区域。
    #[arg(long = "formula-detector", value_name = "ONNX")]
    detector: Option<PathBuf>,
    /// 公式识别模型 SHA-256 校验。
    #[arg(long = "formula-sha256")]
    sha256: Option<String>,
    /// 公式检测置信度阈值。
    #[arg(long = "formula-confidence", default_value_t = 0.25)]
    confidence: f32,
    /// 每页最多保留的公式区域数。
    #[arg(long = "formula-max-regions", default_value_t = 64)]
    max_regions: usize,
    /// 在 JSON 输出中保留公式原始 token IDs。
    #[arg(long = "formula-token-ids")]
    token_ids: bool,
}

impl FormulaArgs {
    fn policy(&self) -> FormulaPolicy {
        match &self.model {
            Some(model) => FormulaPolicy {
                enabled: true,
                model_path: Some(model.clone()),
                expected_model_sha256: self.sha256.clone(),
                detector_path: self.detector.clone(),
                confidence_threshold: self.confidence,
                max_regions: self.max_regions,
                include_token_ids: self.token_ids,
                ..FormulaPolicy::default()
            },
            None => FormulaPolicy::default(),
        }
    }
}

#[derive(Debug, Parser)]
#[command(name = "rapidocr", about = "Run PaddleOCR ONNX models")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Debug, Subcommand)]
enum Command {
    Run {
        #[arg(long, value_name = "IMAGE")]
        img_path: PathBuf,
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        words: bool,
        #[arg(long)]
        chars: bool,
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        formula: FormulaArgs,
    },
    Report {
        #[arg(long = "input-dir")]
        input_dir: PathBuf,
        #[arg(long = "output-dir")]
        output_dir: PathBuf,
        #[arg(long)]
        config: Option<PathBuf>,
        #[command(flatten)]
        formula: FormulaArgs,
    },
    Evaluate {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long, default_value_t = 0.5)]
        iou_threshold: f32,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    Check,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    match Cli::parse().command {
        Command::Run {
            img_path,
            config,
            words,
            chars,
            json,
            formula,
        } => {
            let cfg = match config {
                Some(path) => EngineConfig::from_yaml_file(path)?,
                None => EngineConfig::default(),
            };
            let mut engine = RapidOcrEngine::new(cfg)?;
            let bytes = std::fs::read(&img_path)?;
            let word_mode = if chars {
                WordOutputMode::Chars
            } else if words {
                WordOutputMode::Words
            } else {
                WordOutputMode::Off
            };
            let mut request = make_request(bytes, word_mode);
            request.formula = formula.policy();
            let output = engine.recognize(request)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&to_output_json(&output)?)?
                );
            } else {
                println!("{}", output.plain_text(TextOrder::Reading));
                for (index, latex) in output.formula_latex(TextOrder::Reading) {
                    println!("formula[{index}] = {latex}");
                }
                println!(
                    "regions={} formulas={} total_ms={:.1}",
                    output.regions.len(),
                    output.formula_count(),
                    output.timings.total_ms
                );
            }
        }
        Command::Check => println!("rapid-ocr-rs ready"),
        Command::Report {
            input_dir,
            output_dir,
            config,
            formula,
        } => report_cmd(input_dir, output_dir, config, formula.policy())?,
        Command::Evaluate {
            manifest,
            config,
            iou_threshold,
            output,
        } => evaluate_cmd(manifest, config, iou_threshold, output)?,
    }
    Ok(())
}

fn make_request(bytes: Vec<u8>, words: WordOutputMode) -> OcrRequest {
    OcrRequest {
        input: ImageInput::Encoded(Arc::from(bytes)),
        roi: None,
        scale_hint: None,
        stages: StagePlan {
            detect: true,
            classify: ClassifierPlan {
                policy: ClassifierPolicy::IfAvailable,
                apply_rotation: true,
            },
            recognize: true,
        },
        preprocess: PreprocessPolicy::default(),
        detection: DetectionPolicy::default(),
        recognition: RecognitionPolicy { words },
        output: OutputPolicy::default(),
        formula: FormulaPolicy::default(),
    }
}

fn report_cmd(
    input_dir: PathBuf,
    output_dir: PathBuf,
    config: Option<PathBuf>,
    formula: FormulaPolicy,
) -> Result<(), Box<dyn std::error::Error>> {
    let cfg = match config {
        Some(path) => EngineConfig::from_yaml_file(path)?,
        None => EngineConfig::default(),
    };
    let mut engine = RapidOcrEngine::new(cfg)?;
    std::fs::create_dir_all(&output_dir)?;
    for entry in std::fs::read_dir(&input_dir)? {
        let path = entry?.path();
        if !path.is_file() {
            continue;
        }
        let Some(ext) = path.extension().and_then(|value| value.to_str()) else {
            continue;
        };
        if !matches!(
            ext.to_ascii_lowercase().as_str(),
            "png" | "jpg" | "jpeg" | "bmp" | "webp"
        ) {
            continue;
        }
        let mut request = make_request(std::fs::read(&path)?, WordOutputMode::Off);
        request.formula = formula.clone();
        let output = engine.recognize(request)?;
        let source_name = path
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("source.png");
        let html_name = format!(
            "{}.html",
            path.file_stem()
                .and_then(|v| v.to_str())
                .unwrap_or("report")
        );
        std::fs::copy(&path, output_dir.join(source_name))?;
        let html = render_output_report(
            source_name,
            source_name,
            &output,
            &format!("total {:.1} ms", output.timings.total_ms),
        )?;
        std::fs::write(output_dir.join(html_name), html)?;
    }
    Ok(())
}

fn evaluate_cmd(
    manifest: PathBuf,
    config: Option<PathBuf>,
    iou_threshold: f32,
    output: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let cases: Vec<EvaluationCase> = serde_json::from_str(&std::fs::read_to_string(&manifest)?)?;
    let manifest_dir = manifest.parent().unwrap_or_else(|| Path::new("."));
    let cfg = match config {
        Some(path) => EngineConfig::from_yaml_file(path)?,
        None => EngineConfig::default(),
    };
    let mut engine = RapidOcrEngine::new(cfg)?;
    let mut reports = Vec::with_capacity(cases.len());
    for case in cases {
        let image_path = Path::new(&case.image);
        let resolved_image = if image_path.is_absolute() {
            image_path.to_path_buf()
        } else {
            manifest_dir.join(image_path)
        };
        let result = engine.recognize(make_request(
            std::fs::read(&resolved_image)?,
            WordOutputMode::Off,
        ))?;
        let metrics = evaluate_case(
            &case.text,
            &case.boxes,
            &to_output_items(&result),
            iou_threshold,
        );
        reports.push(EvaluationReport {
            image: case.image,
            metrics,
        });
    }
    let summary = EvaluationSummary::from_cases(reports);
    let text = serde_json::to_string_pretty(&summary)?;
    if let Some(path) = output {
        std::fs::write(path, &text)?;
    }
    println!("{text}");
    Ok(())
}

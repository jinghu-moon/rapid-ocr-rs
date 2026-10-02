use clap::{Parser, Subcommand};
use rapid_ocr_rs::evaluation::ocr::{
    EvaluationCase, EvaluationReport, EvaluationSummary, evaluate_case,
};
use rapid_ocr_rs::{
    ClassifierPlan, ClassifierPolicy, DetectionPolicy, EngineConfig, ImageInput, OcrEngine,
    OcrRequest, OutputPolicy, PreprocessPolicy, RapidOcrEngine, RecognitionPolicy, StagePlan,
    TextOrder, WordOutputMode, render_output_report, to_output_items, to_output_json,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

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
    },
    Report {
        #[arg(long = "input-dir")]
        input_dir: PathBuf,
        #[arg(long = "output-dir")]
        output_dir: PathBuf,
        #[arg(long)]
        config: Option<PathBuf>,
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
            let output = engine.recognize(OcrRequest {
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
                recognition: RecognitionPolicy { words: word_mode },
                output: OutputPolicy::default(),
            })?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&to_output_json(&output)?)?
                );
            } else {
                println!("{}", output.plain_text(TextOrder::Reading));
                println!(
                    "regions={} total_ms={:.1}",
                    output.regions.len(),
                    output.timings.total_ms
                );
            }
        }
        Command::Check => println!("rapid-ocr-rs ready"),
        Command::Report {
            input_dir,
            output_dir,
            config,
        } => report_cmd(input_dir, output_dir, config)?,
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
    }
}

fn report_cmd(
    input_dir: PathBuf,
    output_dir: PathBuf,
    config: Option<PathBuf>,
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
        let output = engine.recognize(make_request(std::fs::read(&path)?, WordOutputMode::Off))?;
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

use clap::Parser;
use rapid_ocr_rs::{
    ClassifierPlan, ClassifierPolicy, DetectionPolicy, EngineConfig, ImageInput, OcrEngine,
    OcrRequest, OutputPolicy, PreprocessPolicy, RapidOcrEngine, RecognitionPolicy, StagePlan,
    WordOutputMode,
};
use serde_json::json;
use std::{fs, path::PathBuf, sync::Arc, time::Instant};

#[derive(Debug, Parser)]
struct Cli {
    #[arg(long = "config")]
    config_path: Option<PathBuf>,
    #[arg(long, default_value = "OCR-test-image")]
    images_dir: PathBuf,
    #[arg(long, default_value_t = 1)]
    rounds: usize,
    #[arg(long, default_value_t = 0)]
    warmup_rounds: usize,
    #[arg(long)]
    output: Option<PathBuf>,
    #[arg(long)]
    max_side_len: Option<usize>,
    #[arg(long)]
    intra_threads: Option<usize>,
}
fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    if cfg!(debug_assertions) {
        eprintln!(
            "warning: bench_warm_e2e is running a debug build; use `cargo run --release` for performance comparisons"
        );
    }
    let mut images = Vec::new();
    for e in fs::read_dir(&cli.images_dir)? {
        let p = e?.path();
        if p.is_file()
            && let Some(ext) = p.extension().and_then(|v| v.to_str())
            && matches!(
                ext.to_ascii_lowercase().as_str(),
                "jpg" | "jpeg" | "png" | "bmp" | "webp" | "tif" | "tiff"
            )
        {
            images.push(p);
        }
    }
    images.sort();
    if images.is_empty() {
        return Err(format!("no images found under {}", cli.images_dir.display()).into());
    }
    let mut cfg = match cli.config_path {
        Some(p) => EngineConfig::from_yaml_file(p)?,
        None => EngineConfig::default(),
    };
    if let Some(v) = cli.max_side_len {
        cfg.global.max_side_len = v;
    }
    if let Some(v) = cli.intra_threads {
        for rt in [
            &mut cfg.det.runtime,
            &mut cfg.cls.runtime,
            &mut cfg.rec.runtime,
        ] {
            rt.intra_threads = Some(v);
            rt.inter_threads = Some(1);
            rt.auto_tune_threads = false;
        }
    }
    let effective_max_side = cfg.global.max_side_len as u32;
    let benchmark_meta = json!({
        "build_profile": if cfg!(debug_assertions) { "debug" } else { "release" },
        "max_side_len": effective_max_side,
        "intra_threads": {
            "det": cfg.det.runtime.intra_threads,
            "cls": cfg.cls.runtime.intra_threads,
            "rec": cfg.rec.runtime.intra_threads,
        },
        "inter_threads": {
            "det": cfg.det.runtime.inter_threads,
            "cls": cfg.cls.runtime.inter_threads,
            "rec": cfg.rec.runtime.inter_threads,
        },
        "auto_tune_threads": {
            "det": cfg.det.runtime.auto_tune_threads,
            "cls": cfg.cls.runtime.auto_tune_threads,
            "rec": cfg.rec.runtime.auto_tune_threads,
        },
        "timing_scope": {
            "wall_ms": "file_read_plus_ocr",
            "ocr_total_ms": "ocr_pipeline_only",
        },
    });
    let engine_start = Instant::now();
    let mut engine = RapidOcrEngine::new(cfg)?;
    let init_ms = engine_start.elapsed().as_secs_f64() * 1000.0;
    let request = |bytes: Vec<u8>| OcrRequest {
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
        preprocess: PreprocessPolicy {
            max_side: Some(effective_max_side),
            ..PreprocessPolicy::default()
        },
        detection: DetectionPolicy::default(),
        recognition: RecognitionPolicy {
            words: WordOutputMode::Off,
        },
        output: OutputPolicy::default(),
        formula: rapid_ocr_rs::FormulaPolicy::default(),
    };
    for _ in 0..cli.warmup_rounds {
        for p in &images {
            let _ = engine.recognize(request(fs::read(p)?))?;
        }
    }
    let mut wall = Vec::new();
    let mut total = Vec::new();
    let mut regions = Vec::new();
    // 记录实际解析到的 provider 与是否发生 CPU fallback：GPU 基准不得把
    // fallback 当加速成功（阶段 0/2/5 的证据要求）。
    let mut provider_resolution = None;
    for _ in 0..cli.rounds {
        for p in &images {
            let start = Instant::now();
            let out = engine.recognize(request(fs::read(p)?))?;
            wall.push(start.elapsed().as_secs_f64() * 1000.0);
            total.push(out.timings.total_ms as f64);
            regions.push(out.regions.len() as f64);
            if provider_resolution.is_none() {
                let describe = |info: &rapid_ocr_rs::ProviderResolutionInfo| {
                    serde_json::json!({
                        "requested": format!("{:?}", info.requested),
                        "resolved": format!("{:?}", info.resolved),
                        "fallback_to_cpu": info.fallback_to_cpu,
                    })
                };
                provider_resolution = Some(serde_json::json!({
                    "model_id": out.engine.model_id,
                    "detector": describe(&out.engine.provider.detector),
                    "classifier": out.engine.provider.classifier.as_ref().map(&describe),
                    "recognizer": describe(&out.engine.provider.recognizer),
                }));
            }
        }
    }
    let report = json!({
        "meta": {
            "images_dir": cli.images_dir,
            "image_count": images.len(),
            "rounds": cli.rounds,
            "warmup_rounds": cli.warmup_rounds,
            // 启动时间：`RapidOcrEngine::new` 的墙钟耗时（含模型加载与会话创建）。
            "init_ms": init_ms,
            "benchmark": benchmark_meta,
            "provider_resolution": provider_resolution,
            // 实际加载的 ONNX Runtime 版本：本 crate 链接导入库，运行时可能加载系统
            // 自带的 `onnxruntime.dll`，这决定了哪些加速 provider 真正可用。
            "ort_runtime_version": rapid_ocr_rs::ort_runtime_version(),
        },
        "stats": {
            "wall_ms": stats(&wall),
            "ocr_total_ms": stats(&total),
            "regions": stats(&regions),
        },
        // 峰值工作集口径与库内一致：`windows:GetProcessMemoryInfo.PeakWorkingSetSize`。
        "memory": {
            "peak_working_set_bytes": rapid_ocr_rs::peak_working_set_bytes(),
            "source": rapid_ocr_rs::peak_memory_source(),
        },
    });
    let text = serde_json::to_string_pretty(&report)?;
    if let Some(p) = cli.output {
        fs::write(&p, &text)?;
    }
    println!("{text}");
    Ok(())
}
fn stats(v: &[f64]) -> serde_json::Value {
    if v.is_empty() {
        return json!({"count":0});
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    let avg = s.iter().sum::<f64>() / s.len() as f64;
    json!({"count":s.len(),"avg":avg,"p50":s[(s.len()-1)/2],"p90":s[((s.len()-1) as f64*0.9).round() as usize],"min":s[0],"max":s[s.len()-1]})
}

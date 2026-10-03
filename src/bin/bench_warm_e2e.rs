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
    // `--intra-threads` 仍然是矩阵脚本的入口，但它现在设置的是**唯一一份**运行时配置：
    // 三个阶段共享同一个 plan（见 `runtime::profile`）。
    if let Some(v) = cli.intra_threads {
        cfg.runtime.intra_threads = Some(v);
        cfg.runtime.inter_threads = Some(1);
        cfg.runtime.auto_tune_threads = false;
    }
    let effective_max_side = cfg.global.max_side_len as u32;
    // 配置里“请求”的运行时设置。实际生效的线程分配在 `meta.thread_plan`，
    // 由引擎解析后回报（例如 auto 的 Rayon 份额可能被已有全局池改写成实际值）。
    let runtime_config = serde_json::to_value(&cfg.runtime)?;
    let benchmark_meta = json!({
        "build_profile": if cfg!(debug_assertions) { "debug" } else { "release" },
        "max_side_len": effective_max_side,
        "runtime": runtime_config,
        "timing_scope": {
            "wall_ms": "file_read_plus_ocr",
            "ocr_total_ms": "ocr_pipeline_only",
        },
    });
    let engine_start = Instant::now();
    let mut engine = RapidOcrEngine::new(cfg)?;
    let init_ms = engine_start.elapsed().as_secs_f64() * 1000.0;
    // 引擎解析出的线程 plan：报告里的数字是**生效值**，不是 YAML 里写了什么。
    let thread_plan = serde_json::to_value(&engine.runtime_profile().threads)?;
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
    // 分阶段耗时：每个样本的 `OcrOutput::timings` / `OcrOutput::stages` 都是**同一次
    // 请求内**的分阶段计时，因此这些样本可以独立统计，不需要把一次请求的内部阶段
    // 互相加减。检测器/分类器/识别器各自的 preprocess / infer / postprocess 分开收集，
    // 使报告能直接回答“时间花在 ORT 推理上还是 Rust 前后处理上”。
    let mut stages = StageSamples::default();
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
            stages.push(&out);
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
    let stage_report = stages.into_report(total.len());
    // 总耗时均值与分阶段均值来自同一批样本，因此可以安全相除得到时间占比。
    let total_avg = if total.is_empty() {
        0.0
    } else {
        total.iter().sum::<f64>() / total.len() as f64
    };
    let timing_split = inference_share(&stage_report, total_avg);
    let report = json!({
        "meta": {
            "images_dir": cli.images_dir,
            "image_count": images.len(),
            "rounds": cli.rounds,
            "warmup_rounds": cli.warmup_rounds,
            // 启动时间：`RapidOcrEngine::new` 的墙钟耗时（含模型加载与会话创建）。
            "init_ms": init_ms,
            "benchmark": benchmark_meta,
            // 引擎解析出的线程分配（生效值；三个阶段共享同一个 plan）。
            "thread_plan": thread_plan,
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
        // 分阶段统计：每个指标都带 count / min / max / avg / p50 / p90，口径与
        // `stats.wall_ms` / `stats.ocr_total_ms` 完全一致（同一个 `stats()` helper）。
        "stages": stage_report,
        // ONNX Runtime 推理 vs Rust 前后处理的时间占比（阶段 6 门槛证据）。
        "timing_split": timing_split,
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

/// `None` 计时（某个阶段没有运行）不得被当成 0 ms 混进统计。
///
/// `OcrOutput` 用 `Option<f32>` 表达“该阶段没有执行”，例如关闭分类器后
/// `classifier_infer_ms` 为 `None`。把 `None` 折算成 `0.0` 会把“未运行”和
/// “运行了但耗时为零”混为一谈，从而系统性拉低均值。
#[derive(Default)]
struct StageSamples {
    input_decode_ms: Vec<f64>,
    input_resize_ms: Vec<f64>,
    input_crop_ms: Vec<f64>,
    detector_preprocess_ms: Vec<f64>,
    detector_infer_ms: Vec<f64>,
    detector_postprocess_ms: Vec<f64>,
    classifier_preprocess_ms: Vec<f64>,
    classifier_infer_ms: Vec<f64>,
    classifier_postprocess_ms: Vec<f64>,
    recognizer_preprocess_ms: Vec<f64>,
    recognizer_infer_ms: Vec<f64>,
    recognizer_postprocess_ms: Vec<f64>,
    preprocess_ms: Vec<f64>,
    postprocess_ms: Vec<f64>,
}

impl StageSamples {
    fn push(&mut self, out: &rapid_ocr_rs::OcrOutput) {
        let push_opt = |dst: &mut Vec<f64>, value: Option<f32>| {
            if let Some(value) = value {
                dst.push(f64::from(value));
            }
        };
        push_opt(&mut self.input_decode_ms, Some(out.timings.decode_ms));
        push_opt(&mut self.input_resize_ms, Some(out.timings.resize_ms));
        push_opt(&mut self.input_crop_ms, Some(out.timings.crop_ms));

        push_opt(
            &mut self.detector_preprocess_ms,
            out.stages.detector.timing.map(|t| t.preprocess_ms),
        );
        push_opt(
            &mut self.detector_infer_ms,
            out.stages.detector.timing.map(|t| t.infer_ms),
        );
        push_opt(
            &mut self.detector_postprocess_ms,
            out.stages.detector.timing.map(|t| t.postprocess_ms),
        );

        push_opt(
            &mut self.classifier_preprocess_ms,
            out.stages.classifier.timing.map(|t| t.preprocess_ms),
        );
        push_opt(
            &mut self.classifier_infer_ms,
            out.stages.classifier.timing.map(|t| t.infer_ms),
        );
        push_opt(
            &mut self.classifier_postprocess_ms,
            out.stages.classifier.timing.map(|t| t.postprocess_ms),
        );

        push_opt(
            &mut self.recognizer_preprocess_ms,
            out.stages.recognizer.timing.map(|t| t.preprocess_ms),
        );
        push_opt(
            &mut self.recognizer_infer_ms,
            out.stages.recognizer.timing.map(|t| t.infer_ms),
        );
        push_opt(
            &mut self.recognizer_postprocess_ms,
            out.stages.recognizer.timing.map(|t| t.postprocess_ms),
        );

        // 顶层 preprocess_ms / postprocess_ms 已经跨阶段求和，是“Rust 侧”总账。
        self.preprocess_ms
            .push(f64::from(out.timings.preprocess_ms));
        self.postprocess_ms
            .push(f64::from(out.timings.postprocess_ms));
    }

    fn into_report(self, samples: usize) -> serde_json::Value {
        json!({
            "unit": "ms",
            "samples": samples,
            "input": {
                "decode_ms": stats(&self.input_decode_ms),
                "resize_ms": stats(&self.input_resize_ms),
                "crop_ms": stats(&self.input_crop_ms),
            },
            "detector": {
                "preprocess_ms": stats(&self.detector_preprocess_ms),
                "infer_ms": stats(&self.detector_infer_ms),
                "postprocess_ms": stats(&self.detector_postprocess_ms),
            },
            "classifier": {
                "preprocess_ms": stats(&self.classifier_preprocess_ms),
                "infer_ms": stats(&self.classifier_infer_ms),
                "postprocess_ms": stats(&self.classifier_postprocess_ms),
            },
            "recognizer": {
                "preprocess_ms": stats(&self.recognizer_preprocess_ms),
                "infer_ms": stats(&self.recognizer_infer_ms),
                "postprocess_ms": stats(&self.recognizer_postprocess_ms),
            },
            // 跨阶段总账：`preprocess_ms` 是三个阶段的 preprocess 之和，
            // `postprocess_ms` 同理。两者都不包含 ORT 推理时间。
            "page_total": {
                "preprocess_ms": stats(&self.preprocess_ms),
                "postprocess_ms": stats(&self.postprocess_ms),
            },
        })
    }
}

/// ONNX Runtime 推理 vs Rust 前后处理的时间占比。
///
/// 分子/分母都取自 `stats.ocr_total_ms.avg`（同一个口径，来自 `OcrTimings::total_ms`），
/// 而不是把分阶段均值相加——阶段均值相加会丢失阶段间的重叠或未计部分。
fn inference_share(stages: &serde_json::Value, total_avg: f64) -> serde_json::Value {
    let avg = |path: &[&str]| -> f64 {
        let mut cursor = stages;
        for key in path {
            cursor = match cursor.get(*key) {
                Some(value) => value,
                None => return 0.0,
            };
        }
        cursor
            .get("avg")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0)
    };
    if total_avg <= 0.0 {
        return json!({"error": "ocr_total_ms.avg is zero; cannot compute shares"});
    }
    let detector_infer = avg(&["detector", "infer_ms"]);
    let classifier_infer = avg(&["classifier", "infer_ms"]);
    let recognizer_infer = avg(&["recognizer", "infer_ms"]);
    let inference = detector_infer + classifier_infer + recognizer_infer;
    let preprocess = avg(&["page_total", "preprocess_ms"]);
    let postprocess = avg(&["page_total", "postprocess_ms"]);
    let share = |value: f64| value / total_avg;
    json!({
        "basis": "mean (avg) of the same samples used by stats.ocr_total_ms",
        "denominator_ms": total_avg,
        "ort_inference_ms": inference,
        "ort_inference_share": share(inference),
        "detector_infer_share": share(detector_infer),
        "classifier_infer_share": share(classifier_infer),
        "recognizer_infer_share": share(recognizer_infer),
        "rust_preprocess_ms": preprocess,
        "rust_preprocess_share": share(preprocess),
        "rust_postprocess_ms": postprocess,
        "rust_postprocess_share": share(postprocess),
        "rust_total_share": share(preprocess + postprocess),
        "unattributed_ms": total_avg - inference - preprocess - postprocess,
        "unattributed_share": share(total_avg - inference - preprocess - postprocess),
    })
}

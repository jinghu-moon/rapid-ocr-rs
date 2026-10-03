use clap::Parser;
use rapid_ocr_rs::{
    ClassifierPlan, ClassifierPolicy, DetectionPolicy, EngineConfig, ImageInput, OcrEngine,
    OcrRequest, OutputPolicy, PreprocessPolicy, RapidOcrEngine, RecognitionPolicy, StagePlan,
    TimingLedger, WordOutputMode,
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
    // 时间账本：每个样本各自算一份（每一项只算一次），最后按字段求均值，
    // 再在均值账本上做守恒检查。`timing_ledger.conservation` 是守恒判据本身。
    let mut ledgers: Vec<TimingLedger> = Vec::new();
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
            ledgers.push(TimingLedger::from_timings(&out.timings));
            if provider_resolution.is_none() {
                let describe = |info: &rapid_ocr_rs::ProviderResolutionInfo| {
                    serde_json::json!({
                        "requested": format!("{:?}", info.requested),
                        // `selected_ep` 只表示“交给 ORT 的 EP 链头部”，不是逐节点执行证据。
                        "selected_ep": format!("{:?}", info.selected_ep),
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
    let ledger_report = ledger_report(&ledgers);
    let timing_split = inference_share(&ledgers, total_avg);
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
            // `null` 表示该值未被配置（交给 ORT / Rayon 自己的默认值）。
            "thread_plan": thread_plan,
            "provider_resolution": provider_resolution,
            // 实际使用的 ONNX Runtime 指纹：API 版本字符串 + 运行库文件（路径/体积/SHA-256）
            // + provider DLL。本 crate 用 `rustc-link-lib=static=onnxruntime` 链接，
            // 进程里没有 onnxruntime.dll 模块，因此指纹来自静态库或 exe（见
            // `runtime::ort_runtime`）。没有指纹的报告不可与其它报告严格比较。
            "ort_runtime": rapid_ocr_rs::ort_runtime_fingerprint(),
            // 版本字符串仍然单独保留（旧报告里叫 `ort_runtime_version`），
            // 让“版本”这一项可以直接被 grep 到，不必先理解整个指纹结构。
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
        // 时间账本：每一项只算一次，余量显式列出，并给出守恒判定 + **解释**
        // （`conservation.interpretation`）。它是**诊断**仪器：分量来自跨越
        // `inner.run()` 的重叠窗口，因此占比只在 `conservation.overlap_ms` 内成立，
        // 不能作为性能验收依据。
        "timing_ledger": ledger_report,
        // ORT 推理 vs Rust 前后处理的时间占比（阶段 6 门槛证据的来源之一）；
        // 数值与 `timing_ledger` 一致，保留这个键是为了让旧的对比方式仍然可用。
        // 与 `timing_ledger` 一样带着残差量级，读取时必须一起看。
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

/// 守恒时间账本报告。
///
/// 每个样本各算一份账（[`TimingLedger::from_timings`]，每一项只算一次），再按字段求
/// 均值。守恒检查建立在均值账本上，而**均值账本与每个样本账本一样并不严格守恒**：
/// 外层 `preprocess_ms` 窗口与阶段计时跨越 `inner.run()`，分量不是互斥窗口。
///
/// 因此这里必须把“不守恒该怎么读”一起写进 JSON：
/// `conservation.interpretation` 明确写出“负残差 = 窗口重叠、账本是诊断工具、占比只在
/// 残差量级内成立、`total_ms` 本身不受影响”，`conservation.overlap_ms` 给出该量级。
/// 报告的使用者**不得**把账本当作性能验收依据，也不得把 `conserved = false` 读成
/// “总量算错了”。
fn ledger_report(ledgers: &[TimingLedger]) -> serde_json::Value {
    if ledgers.is_empty() {
        return json!({
            "unit": "ms",
            "samples": 0,
            "error": "no timing samples were collected",
        });
    }
    let mean = TimingLedger::mean(ledgers);
    let conservation = mean.conservation();
    let shares = mean.shares();
    json!({
        "unit": "ms",
        "samples": ledgers.len(),
        "basis": "per-sample ledger, then per-field mean (each component counted exactly once)",
        "components": mean,
        "totals": {
            "input_ms": mean.input_ms(),
            "model_preprocess_ms": mean.model_preprocess_ms(),
            "inference_ms": mean.inference_ms(),
            "model_postprocess_ms": mean.model_postprocess_ms(),
            "page_postprocess_ms": mean.page_postprocess_ms,
            "formula_ms": mean.formula_ms,
            "rust_ms": mean.rust_ms(),
            "attributed_ms": mean.attributed_ms(),
            "unattributed_ms": mean.unattributed_ms,
        },
        "shares": shares,
        // 守恒判据 + **怎么读它**：`conservation.interpretation` 说明负残差是计时窗口重叠
        // （诊断仪器的局限），不是 `total_ms` 算错了；`overlap_ms` 是占比成立的上界。
        // 这个账本是诊断工具，不能作为性能验收依据。
        "conservation": conservation,
        // 每个样本各自的余量：用来看“均值守恒”是不是掩盖了单样本的大偏差。
        "per_sample_unattributed_ms": stats(
            &ledgers.iter().map(|ledger| ledger.unattributed_ms).collect::<Vec<_>>()
        ),
        "per_sample_residual_ms": stats(
            &ledgers
                .iter()
                .map(|ledger| ledger.conservation().residual_ms)
                .collect::<Vec<_>>()
        ),
    })
}

/// ONNX Runtime 推理 vs Rust 前后处理的时间占比。
///
/// 分子来自时间账本的均值，分母是 `stats.ocr_total_ms.avg`（同一个口径，来自
/// `OcrTimings::total_ms`），因此 `inference + rust + unattributed == total` 按构造成立。
///
/// 按构造成立**不等于**这些占比是精确划分：账本的各个分量来自互相重叠的计时窗口
/// （见 `timing_ledger.conservation`），所以每一项占比只在
/// `conservation.overlap_ms` 的量级内成立（release 实测 ±0.69%）。这个量级足以支撑
/// “瓶颈在 ORT（比每个 Rust 分量大一个数量级）”，但不足以支撑更细的性能结论。
fn inference_share(ledgers: &[TimingLedger], total_avg: f64) -> serde_json::Value {
    if ledgers.is_empty() {
        return json!({"error": "no timing samples were collected"});
    }
    if total_avg <= 0.0 {
        return json!({"error": "ocr_total_ms.avg is zero; cannot compute shares"});
    }
    let mean = TimingLedger::mean(ledgers);
    let inference = mean.inference_ms();
    let rust = mean.rust_ms();
    let share = |value: f64| value / total_avg;
    json!({
        "basis": "per-sample ledger mean; the same samples back stats.ocr_total_ms",
        "denominator_ms": total_avg,
        "ort_inference_ms": inference,
        "ort_inference_share": share(inference),
        "detector_infer_share": share(mean.detector_infer_ms),
        "classifier_infer_share": share(mean.classifier_infer_ms),
        "recognizer_infer_share": share(mean.recognizer_infer_ms),
        "rust_ms": rust,
        "rust_share": share(rust),
        // 兼容旧的字段名：以前把“页面级 preprocess + postprocess”称作 rust_preprocess /
        // rust_postprocess。这两个数字现在**不再是**那两项（那两项本身漏项且重复），
        // 而是时间账本里的输入侧与后处理侧，读旧字段名的人必须知道这一点。
        "rust_input_ms": mean.input_ms(),
        "rust_input_share": share(mean.input_ms()),
        "rust_model_preprocess_ms": mean.model_preprocess_ms(),
        "rust_model_preprocess_share": share(mean.model_preprocess_ms()),
        "rust_model_postprocess_ms": mean.model_postprocess_ms(),
        "rust_model_postprocess_share": share(mean.model_postprocess_ms()),
        "rust_page_postprocess_ms": mean.page_postprocess_ms,
        "rust_page_postprocess_share": share(mean.page_postprocess_ms),
        "unattributed_ms": mean.unattributed_ms,
        "unattributed_share": share(mean.unattributed_ms),
    })
}

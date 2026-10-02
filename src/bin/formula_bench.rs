use clap::{Parser, ValueEnum};
use rapid_ocr_rs::{
    FormulaPreprocessor, FormulaSession, FormulaTokenizer, FormulaTokenizerMetadata,
    ProviderPreference, RuntimeConfig,
};
use serde::Serialize;
use std::path::PathBuf;
use std::time::Instant;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ProviderArg {
    Cpu,
    Directml,
    Cuda,
}

#[derive(Debug, Parser)]
struct Cli {
    #[arg(long)]
    model: PathBuf,
    #[arg(long)]
    image: PathBuf,
    #[arg(long, default_value_t = 5)]
    rounds: usize,
    #[arg(long, value_enum, default_value_t = ProviderArg::Cpu)]
    provider: ProviderArg,
}

#[derive(Debug, Default, Serialize)]
struct Timing {
    session_create_ms: f64,
    first_inference_ms: f64,
    preprocess_ms: f64,
    run_ms: f64,
    decode_ms: f64,
    e2e_ms: f64,
}

#[derive(Debug, Serialize)]
struct BatchTiming {
    batch: usize,
    preprocess_ms: f64,
    run_ms: f64,
    decode_ms: f64,
    e2e_ms: f64,
}

#[derive(Debug, Serialize)]
struct Report {
    provider: String,
    provider_resolution: String,
    rounds: usize,
    single: Timing,
    batches: Vec<BatchTiming>,
    error: Option<String>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn runtime_config(provider: ProviderArg) -> RuntimeConfig {
    let mut config = RuntimeConfig::default();
    config.provider_preference = match provider {
        ProviderArg::Cpu => ProviderPreference::Cpu,
        ProviderArg::Directml => ProviderPreference::DirectMl { device_id: 0 },
        ProviderArg::Cuda => ProviderPreference::Cuda { device_id: 0 },
    };
    config.fail_if_provider_unavailable = true;
    config
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let runtime = runtime_config(cli.provider);
    let image = image::open(&cli.image)?;

    let session_start = Instant::now();
    let mut session = FormulaSession::new(&cli.model, &runtime)?;
    let session_create_ms = ms(session_start);
    let provider_resolution = format!("{:?}", session.provider_resolution());
    let character_metadata = session
        .character_metadata()?
        .ok_or("model has no character metadata")?;
    let metadata = FormulaTokenizerMetadata::from_character_metadata(&character_metadata)?;
    let tokenizer = FormulaTokenizer::from_metadata(&metadata)?;
    let preprocessor = FormulaPreprocessor::new();

    let input = preprocessor.preprocess(&image)?;
    let first_start = Instant::now();
    let first_output = session.run(input.view())?;
    let first_inference_ms = ms(first_start);
    let _ = tokenizer.decode_ids(&first_output.row(0).to_vec())?;

    let mut single = Timing {
        session_create_ms,
        first_inference_ms,
        ..Timing::default()
    };
    for _ in 0..cli.rounds {
        let preprocess_start = Instant::now();
        let tensor = preprocessor.preprocess(&image)?;
        let preprocess_ms = ms(preprocess_start);

        let run_start = Instant::now();
        let output = session.run(tensor.view())?;
        let run_ms = ms(run_start);

        let decode_start = Instant::now();
        let decoded = tokenizer.decode_ids(&output.row(0).to_vec())?;
        let decode_ms = ms(decode_start);
        let e2e_ms = preprocess_ms + run_ms + decode_ms;
        let _ = decoded;

        single.preprocess_ms += preprocess_ms;
        single.run_ms += run_ms;
        single.decode_ms += decode_ms;
        single.e2e_ms += e2e_ms;
    }
    if cli.rounds > 0 {
        let divisor = cli.rounds as f64;
        single.preprocess_ms /= divisor;
        single.run_ms /= divisor;
        single.decode_ms /= divisor;
        single.e2e_ms /= divisor;
    }

    let mut batches = Vec::new();
    for batch in [1usize, 2, 4, 8] {
        let images: Vec<_> = std::iter::repeat_with(|| image.clone())
            .take(batch)
            .collect();
        let preprocess_start = Instant::now();
        let tensor = preprocessor.preprocess_batch(&images)?;
        let preprocess_ms = ms(preprocess_start);
        let run_start = Instant::now();
        let output = session.run(tensor.view())?;
        let run_ms = ms(run_start);
        let decode_start = Instant::now();
        for row in output.axis_iter(ndarray::Axis(0)) {
            let _ = tokenizer.decode_ids(&row.to_vec())?;
        }
        let decode_ms = ms(decode_start);
        batches.push(BatchTiming {
            batch,
            preprocess_ms,
            run_ms,
            decode_ms,
            e2e_ms: preprocess_ms + run_ms + decode_ms,
        });
    }

    let report = Report {
        provider: format!("{:?}", cli.provider),
        provider_resolution,
        rounds: cli.rounds,
        single,
        batches,
        error: None,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

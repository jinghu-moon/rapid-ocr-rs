# rapid-ocr-rs

`rapid-ocr-rs` is a reusable Rust OCR crate for PaddleOCR-family ONNX models.
It owns image normalization, detection, text-region cropping, recognition,
CTC decoding, polygon output, model resolution, and ONNX Runtime sessions. It
does not depend on Tauri, SQLite, a clipboard implementation, or a specific
application.

## License

The `rapid-ocr-rs` source code is licensed under the Apache License, Version
2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE). Third-party dependencies,
ONNX Runtime binaries, OCR model weights, and model dictionaries have separate
license and redistribution terms; see [THIRD_PARTY_NOTES.md](THIRD_PARTY_NOTES.md)
before redistributing a complete application or model bundle.

## Current baseline

- PP-OCRv6 detector + recognizer; direction classifier disabled unless a
  compatible classifier artifact is explicitly configured.
- CPU is the default provider. DirectML, CUDA, and CANN are opt-in Cargo
  features and must be benchmarked on the target machine.
- The stable application-neutral API is `OcrEngine::recognize(OcrRequest)`.
  `OwnedPixelBuffer` accepts BGRA/RGBA/RGB/Gray pixels, padding stride,
  bottom-up buffers, and an optional ROI.
- `ImageInput` supports encoded bytes, file paths, synchronous URL loading with
  connect/request timeouts, decoded `RecImage`, and strided pixel buffers.
  Applications that must avoid network access should use `Encoded`, `Pixels`,
  `Image`, or `File` inputs.
- `OcrOutput` contains text, line polygons, scores, provider resolution, and
  stage timings. Character-level boxes are intentionally optional.

## Minimal usage

```rust,no_run
use rapid_ocr_rs::{
    ClassifierPlan, ClassifierPolicy, DetectionPolicy, EngineConfig, ImageInput,
    OcrEngine, OcrRequest, OutputPolicy, PreprocessPolicy, RapidOcrEngine,
    RecognitionPolicy, StagePlan, WordOutputMode,
};

let mut config = EngineConfig::default();
config.global.use_cls = false;
config.det.model_path = Some("PP-OCRv6_det_medium.onnx".into());
config.det.allow_download = false;
config.rec.model.model_path = Some("PP-OCRv6_rec_medium.onnx".into());
config.rec.model.rec_keys_path = Some("ppocrv6_dict.txt".into());
config.rec.model.allow_download = false;
let mut engine = RapidOcrEngine::new(config)?;
let output = engine.recognize(OcrRequest {
    input: ImageInput::File("screen.png".into()),
    roi: None,
    scale_hint: None,
    stages: StagePlan {
        detect: true,
        classify: ClassifierPlan { policy: ClassifierPolicy::Off, apply_rotation: false },
        recognize: true,
    },
    preprocess: PreprocessPolicy::default(),
    detection: DetectionPolicy::default(),
    recognition: RecognitionPolicy { words: WordOutputMode::Off },
    output: OutputPolicy::default(),
})?;
println!("{} regions", output.regions.len());
# Ok::<(), rapid_ocr_rs::RapidOcrError>(())
```

`PreprocessPolicy::default()` leaves `max_side` as `None`, so the engine's
`global.max_side_len` controls resizing. Set `preprocess.max_side` only when a
request needs to override that limit. Encoded bytes, files, and URLs are
dimension-probed and checked against `max_decode_pixels` before pixel decode.
File and URL inputs are additionally bounded by `max_encoded_bytes` (default
128 MiB, `PreprocessPolicy`), with `Content-Length` checks and a streaming read
cap for HTTP responses.

All new integrations should use the generic `OcrEngine` trait and
`OcrRequest`. The former scattered `run`/`OcrResult` API is internal and is not
part of the public crate contract.

## Formula recognition

`rapid-ocr-rs` also exposes an independent PP-FormulaNet_plus-M ONNX formula
API. It does not reuse the CTC `Recognizer` path.

```rust,no_run
use rapid_ocr_rs::{FormulaRecognizer, RuntimeConfig};

let mut recognizer = FormulaRecognizer::from_model(
    "pp_formulanet_plus_m.onnx".as_ref(),
    &RuntimeConfig::default(),
)?;
let result = recognizer.recognize(&image::open("formula.png")?)?;
println!("{}", result.latex);
# Ok::<(), Box<dyn std::error::Error>>(())
```

- `FormulaRecognizer::recognize_batch` preserves input order and defaults to a
  maximum batch size of 16.
- `FormulaRecognition` contains `latex`, raw `token_ids`, `eos_index`,
  `truncated`, model id, and elapsed time.
- `recognize_encoded`, `recognize_file`, and `recognize_url` apply encoded-byte,
  decoded-pixel, streaming-read, and sequence-length limits.
- CPU `CPUExecutionProvider` is the first supported provider. DirectML/CUDA
  support is model/provider dependent and must not be assumed.

Model asset:

```text
https://www.modelscope.cn/models/RapidAI/RapidDoc/resolve/v1.0.0/formula/PP-FormulaNet_plus-M/pp_formulanet_plus_m.onnx
SHA-256 71b6d389cf7b857e45252a4b98cfced1a3ffca7bf24d9497d02d052a41d9493b
```

The model and tokenizer metadata are not bundled. Download them separately,
verify SHA-256, and review upstream licenses before redistribution.

## Model integrity

`ModelManifest::validate_files` verifies every detector, recognizer, dictionary,
and optional classifier file with SHA-256. Keep model weights outside the
crate source tree and record their upstream URL, revision, checksum, and
redistribution terms in the consuming application's manifest.

## CLI and benchmark

### Visual OCR report

The CLI renders one offline HTML report per image in a directory:

```text
cargo run --bin rapidocr -- report \
  --input-dir ../../OCR-test-image \
  --output-dir ./reports/ocr \
  --config ../../OCR-Model/test-config-small.yaml
```

For every supported image in the input directory, the command copies the source
image into the output directory and writes a sibling `.html` report that renders
the image with every OCR polygon in original image coordinates. The report has
no server or external JavaScript dependency and can be opened directly in a
browser. The current CLI does not expose a separate HTML output flag for a
single `run`; use `--json` for one-off machine-readable output or `report` for
the directory-level visual pass.

```text
cargo run --bin rapidocr -- run --img-path screen.png --config config.yaml --json
cargo run --bin bench_warm_e2e -- --config config.yaml --images-dir ./screens --rounds 3
```

For accuracy regression, provide a JSON manifest with `image`, `text`, and
optional quadrilateral `boxes` fields, then run:

```text
cargo run --bin rapidocr -- evaluate \
  --manifest ./golden/manifest.json \
  --config ../../OCR-Model/test-config-small.yaml \
  --output ./reports/evaluation.json
```

The evaluator reports per-image and aggregate Unicode CER, exact-text match,
detection precision and recall, and matched quadrilateral polygon IoU. The
repository fixture `../../OCR-test-image/golden-manifest.json` is generated
from the visible text in `prototypes/ocr-location-test.html`; it is a real
text regression baseline for all 12 images. Its `boxes` arrays are intentionally
empty until every text polygon is independently annotated, so detection
precision/recall/IoU are emitted as `null` rather than being inferred from
predictions.

The benchmark reports model initialization, detector/recognizer timings, and
warm wall-clock P50/P90. For reproducible CPU comparisons, pin the image
resize limit and all ONNX Runtime sessions to the same thread profile:

```text
cargo run --bin bench_warm_e2e -- \
  --config config.yaml --images-dir ./screens \
  --warmup-rounds 1 --rounds 3 \
  --max-side-len 1280 --intra-threads 16
```

`--max-side-len` overrides the global image bound and `--intra-threads`
overrides detector/classifier/recognizer CPU sessions. The benchmark is a
measurement tool, not an accuracy guarantee; compare accuracy separately on
the same image set. GPU providers can be selected explicitly when the
corresponding Cargo feature and runtime libraries are installed:

On the repository's 12-image PP-OCRv6 small set, the warm timing comparison
used the same three-round, 16-thread condition. CER values are from the
corresponding 12-image evaluation runs:

| max side | end-to-end mean | mean CER |
| ---: | ---: | ---: |
| 2000 (quality baseline) | 1016 ms | 0.4477 |
| 1280 (low-latency recommendation) | 666 ms | 0.4355 |
| 960 (aggressive latency) | 610 ms | 0.4525 |

The 1280 setting is therefore exposed as a recommended application profile,
not a changed library default: it was about 34% faster than 2000 on this
machine while the fixture CER did not regress. Re-run the evaluation on the
target corpus before changing an application's quality/latency trade-off.

```text
cargo run --features cuda-provider,download-binaries --bin bench_warm_e2e -- \
  --config config.yaml --provider cuda --device-id 0 \
  --warmup-rounds 1 --rounds 3
```

DirectML is available on Windows with `directml-provider`. Do not assume GPU
is faster: OCR detection produces variable text regions and recognition may
be split into many small dynamic batches. Benchmark CPU, CUDA, and DirectML
on the target image set before selecting an application default. CUDA and
DirectML runs in strict provider mode fail at startup when the requested
provider is unavailable instead of silently falling back to CPU.

On the repository's 12-image benchmark (PP-OCRv6 small, 2000px bound),
DirectML measured about 490 ms/image over three warm rounds. A comparable CPU
16-thread run measured about 1,016 ms/image (the earlier single-round CPU
sample was about 690 ms/image). The same manifest's mean CER was identical
(`0.44765`) for CPU and DirectML. These are machine-specific measurements,
not library-wide guarantees; rerun the benchmark on the target GPU before
changing an application default.

For an application or CLI that wants the ONNX Runtime binaries copied during
build and DirectML enabled, use the opt-in convenience feature:

```text
cargo run --features cli-default --bin rapidocr -- run --img-path screen.png
```

`cli-default` expands to `download-binaries`, `copy-dylibs`, and
`directml-provider`. The library's default feature set remains
`ort-runtime`; it does not download runtime binaries or enable a GPU provider
implicitly.

## API replacement acceptance

Exact numeric equality with the previous implementation is not an acceptance
requirement: the unified API changes region ordering, stage reporting, provider
resolution, and some post-processing details. Replacement acceptance is a
functional regression check with explicit tolerances on the repository fixture
(`OCR-test-image/api-comparison-small-medium.json`):

- mean CER ≤ 0.03 on the 12-image fixture;
- mean polygon IoU ≥ 0.85 where box annotations exist;
- absolute detected-region count delta ≤ 5% versus the old API;
- no per-image hard failure.

Exact-image text match is reported for information only and is not required.
The current small/medium comparison is inside these thresholds.

## Formula benchmark and smoke evaluation

```powershell
# CPU warm timings and batch timing; add --features directml-provider/cuda-provider
# and --provider directml/cuda for provider checks.
cargo run --bin formula_bench -- --model <pp_formulanet_plus_m.onnx> --image <formula.png> --rounds 3 --provider cpu

# Rust token/LaTeX output over a fixture subset.
cargo run --bin formula_compare -- --model <pp_formulanet_plus_m.onnx> --dataset-root ../../Formula-TestSet/ocr_rec_latexocr_dataset_example --split val --limit 100 --output target/formula-rust.json

# Python RapidDoc/ONNX reference for the same subset.
python tools/formula_reference.py --model <pp_formulanet_plus_m.onnx> --dataset-root ../../Formula-TestSet/ocr_rec_latexocr_dataset_example --split val --limit 100 --output target/formula-python.json
python tools/formula_compare_results.py --rust target/formula-rust.json --python target/formula-python.json --output target/formula-compare.json
```

The formula model file is not bundled. Use the RapidDoc `v1.0.0` URL and
SHA-256 recorded in `THIRD_PARTY_NOTES.md`.

## Provider feature verification

Provider features are opt-in and can be compile-checked or tested on a machine
without a GPU:

```text
cargo check --features directml-provider
cargo check --features cuda-provider
cargo check --features directml-provider,cuda-provider,cann-provider
cargo test --features directml-provider
cargo test --features cuda-provider
```

`--all-features` additionally enables `opencv-backend`; that build requires an
OpenCV installation and is not part of the provider-only verification above.

## Implementation reference

The pipeline behavior is cross-checked against RapidOCR and the local
`paddle-ocr-rs` reference. Their licenses and model licenses remain separate
from this crate's Apache-2.0 license; see `THIRD_PARTY_NOTES.md`.

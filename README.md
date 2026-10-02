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
    ClassifierPlan, ClassifierPolicy, DetectionPolicy, EngineConfig, FormulaPolicy, ImageInput,
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
    // Formula routing is off by default, so the text path is byte-identical to a
    // build without the formula feature.
    formula: FormulaPolicy::default(),
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

`rapid-ocr-rs` exposes two formula capabilities: an independent
PP-FormulaNet_plus-M ONNX recognizer, and optional page-level formula routing
that reuses the ordinary OCR pipeline without touching its CTC contract.

### Independent formula API

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
  `truncated`, `model_id`, `elapsed_ms`, and `batch_size`.
  `elapsed_ms` is the wall time of the **call** that produced the result: every
  result of one `recognize_batch` shares the same value (the whole batch), so
  divide by `batch_size` for a per-image estimate. It is never a per-sample
  measurement that grows with position in the batch.
- `recognize_encoded`, `recognize_file`, and `recognize_url` delegate to the
  shared input loader (`input::image_loader`), so encoded-byte, decoded-pixel,
  streaming-read, header-probe, timeout, and error semantics are identical to
  ordinary OCR.
- The default sequence limit is 4096. The model's in-graph `Loop` pads the whole
  batch to 2561 columns when any sample fails to emit EOS, so a limit at or
  below that value would reject an entire batch and lose the samples that did
  recognize correctly. Set a larger limit only if a different model needs it.
- **Provider strictness**: the formula session rejects a silent CPU fallback.
  Requesting an accelerator that is unavailable returns
  `RapidOcrError::UnsupportedProvider` even when
  `RuntimeConfig::fail_if_provider_unavailable` is `false`. Request
  `ProviderPreference::Cpu` explicitly if CPU is intended.

### Page-level formula routing

`OcrRequest::formula` (`FormulaPolicy`) turns a whole page into text regions plus
typed formula regions:

```rust,no_run
use rapid_ocr_rs::{
    FormulaPolicy, ImageInput, OcrEngine, OcrRegion, OcrRequest, RapidOcrEngine, RegionKind,
};

# fn example(engine: &mut RapidOcrEngine) -> Result<(), rapid_ocr_rs::RapidOcrError> {
let request = OcrRequest {
    // ... stages / preprocess / detection / recognition / output as usual ...
    # input: ImageInput::File("page.png".into()),
    # roi: None,
    # scale_hint: None,
    # stages: Default::default(),
    # preprocess: Default::default(),
    # detection: Default::default(),
    # recognition: Default::default(),
    # output: Default::default(),
    formula: FormulaPolicy {
        enabled: true,
        model_path: Some("pp_formulanet_plus_m.onnx".into()),
        detector_path: Some("pix2text-mfd-1.5.onnx".into()),
        ..FormulaPolicy::default()
    },
};
let output = engine.recognize(request)?;
for region in output.regions.iter().filter(|r| r.kind == RegionKind::Formula) {
    println!("{:?}", region.formula.as_ref().map(|f| &f.latex));
}
# Ok(())
# }
```

How routing works:

1. Formula regions come from the optional page detector
   (`pix2text-mfd-1.5.onnx`, a YOLO11 detect model) **and** from
   `FormulaPolicy::input_regions`, which lets a caller declare regions without a
   detector.
2. Candidates are filtered by `min_area_ratio` and deduplicated by
   `iou_threshold` / containment. The result is capped by `max_regions` and is
   independent of candidate order.
3. Formula pixels are painted white **before** the ordinary text pipeline runs,
   so CTC never executes on them. "Formula regions skip CTC" is an execution
   guarantee, not post-hoc filtering.
4. Each formula region is cropped from the untouched original image, recognized
   by `FormulaRecognizer`, and appended as a `RegionKind::Formula` region that
   keeps its polygon, detector score, and model id. Formula regions never carry
   a CTC `recognition` outcome (they have no meaningful per-character
   confidence), and `OcrOutput::validate()` enforces that invariant.
5. `roi` and tiled preprocessing are rejected with a structured error while
   formula routing is enabled, because their coordinate mapping would diverge
   from original-image formula coordinates.

Output formats:

- JSON adds `kind: "text" | "formula"`, `latex`/`eos_index`/`truncated` on
  formula items, and a top-level `formulas` array in reading order.
- Markdown renders formula regions as `$$...$$` display math in reading order.
  Inner `$$` is escaped, blank lines are dropped, and a truncated result gets an
  invisible `<!-- formula truncated: no EOS token -->` marker.
- HTML renders formula polygons separately, lists the LaTeX with a copy button,
  and reports `data-truncated` / `data-eos` per formula.
- Raw token ids are only emitted when `FormulaPolicy::include_token_ids` is set.

Known limitations (explicit, and pinned by tests):

- Detector precision is not perfect. On the repository's 12 smoke pages the
  detector reports false positives on non-formula pages (code listings, dense
  prose) at the default `confidence_threshold = 0.25`. A false-positive region
  removes the text underneath it from the text channel. Raise
  `confidence_threshold`/`min_area_ratio` to trade recall for precision; at
  `0.95` the integration test shows the output returns to the
  formula-disabled baseline.
- A **missed** formula (below threshold, or a detector that returned nothing)
  stays in the text channel, so ordinary CTC will emit garbage for it. There is
  no cross-model arbitration.
- Formula crops use the axis-aligned bounding box of the region quadrilateral.

Model assets:

```text
formula recognizer:
https://www.modelscope.cn/models/RapidAI/RapidDoc/resolve/v1.0.0/formula/PP-FormulaNet_plus-M/pp_formulanet_plus_m.onnx
SHA-256 71b6d389cf7b857e45252a4b98cfced1a3ffca7bf24d9497d02d052a41d9493b

page formula detector:
OCR-Model/Formula-Detection-Model/pix2text-mfd-1.5.onnx (Pix2Text-MFD-1.5, YOLO11m detect)
```

The models and tokenizer metadata are not bundled. Download them separately,
verify SHA-256, and review upstream licenses before redistribution
(`THIRD_PARTY_NOTES.md` records the conflicting license metadata on the detector).

### RapidDoc post-processing equivalence

`FormulaRecognizer` post-processing is `remove_chinese_text_wrapping` ->
`fix_latex` -> `ftfy.fix_text`, matching RapidDoc's order. The deterministic
`ftfy` steps are ported exactly (tables generated from the real `ftfy` by
`tools/build_ftfy_tables.py`, verified byte-for-byte against Python):
`remove_terminal_escapes`, `fix_c1_controls`, `fix_latin_ligatures`,
`fix_character_width`, `uncurl_quotes`, `fix_line_breaks`, NFC normalization, and
`remove_control_characters`.

The heuristic mojibake repairs (`fix_encoding`, `restore_byte_a0`,
`replace_lossy_sequences`, `decode_inconsistent_utf8`) and `unescape_html` are
**not** implemented; `fix_surrogates` cannot apply to a Rust `String`. This is
not a silent gap: `tests/fixtures/formula-postprocess/cases.json` enumerates the
divergence cases and asserts them, and the same fixture records that across all
357,022 labels in `Formula-TestSet` the deterministic subset explains every
observed `ftfy` change (1 of 357,022 lines, a curly-quote uncurl).
`postprocess_latex` must therefore not be described as fully equivalent to
RapidDoc's post-processing.

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

## Formula benchmark and evaluation

`formula_eval` is the phase-9 evaluation tool for all three datasets. It writes a
stable sampling manifest (content-hash sampling, not "first N"), per-sample
records, failure classification, exact/normalized match, CER, EOS/truncated
counts, throughput with P50/P95, peak working set, and provider/thread settings.

```powershell
$Model = "<pp_formulanet_plus_m.onnx>"
$TestSet = "../../Formula-TestSet"   # collection root

# PaddleX example set, 501 val images, with a reproducible manifest.
cargo run --release --bin formula_eval -- --model $Model --dataset-root $TestSet `
  --dataset latexocr --split validate --batch-size 8 `
  --manifest-output target/formula-eval/manifest-val-501.json `
  --output target/formula-eval/val-501.json

# Rust/Python link comparison over exactly the same samples.
python tools/formula_reference.py --model $Model --dataset-root $TestSet `
  --dataset latexocr --manifest target/formula-eval/manifest-val-501.json `
  --output target/formula-eval/python-val-501.json
cargo run --release --bin formula_eval -- --model $Model --dataset-root $TestSet `
  --dataset latexocr --split validate `
  --expect-manifest target/formula-eval/manifest-val-501.json `
  --python-reference target/formula-eval/python-val-501.json `
  --output target/formula-eval/val-501-compared.json

# im2latex smoke and full test set (10,355 references, 71 empty labels).
cargo run --release --bin formula_eval -- --model $Model --dataset-root $TestSet `
  --dataset im2latex --split test --limit 100 --output target/formula-eval/im2latex-100.json
cargo run --release --bin formula_eval -- --model $Model --dataset-root $TestSet `
  --dataset im2latex --split test --output target/formula-eval/im2latex-full.json

# UniMER: one sub-run per subset; HWE is reported separately, never averaged in.
foreach ($subset in 'spe','cpe','sce','hwe') {
  cargo run --release --bin formula_eval -- --model $Model --dataset-root $TestSet `
    --dataset unimer --subset $subset --output "target/formula-eval/unimer-$subset.json"
}

# The whole sequence above, including the CPU benchmark, in one run.
pwsh -NoProfile -File tools/run_formula_evaluation.ps1
```

`formula_eval --expect-manifest` fails if the current selection does not hash to
the recorded manifest, so a re-run either reproduces the same sample set or
reports the difference instead of silently evaluating a different subset.

Benchmark:

```powershell
cargo run --release --bin formula_bench -- --model $Model `
  --image <formula1.png> --image <formula2.png> `
  --rounds 5 --warmup 1 --batch-sizes 1,2,4,8 --provider cpu `
  --output target/formula-eval/bench-cpu.json
```

`formula_bench` reports per-stage min/max/mean/P50/P95/stddev over the measured
rounds, per-image and per-batch end-to-end latency, whether batch size changed
the token sequence, the resolved provider and whether a CPU fallback occurred,
and the process peak working set
(`windows:GetProcessMemoryInfo.PeakWorkingSetSize` or
`linux:/proc/self/status:VmHWM`). Formula throughput is always reported
separately from ordinary OCR; `--ocr-baseline <bench.json>` only adds the
ordinary OCR benchmark side by side and never merges the two into one number.

## Tests and external assets

`cargo test --all-targets` passes in a clean clone without any model or dataset:
the contract fixtures under `tests/fixtures/**` are committed, and every test
that needs the real 594 MB model or the public datasets skips with a message
when the assets are absent. There is no development-machine absolute path
fallback anywhere in the crate.

```powershell
# Force external-asset tests to fail instead of skipping (CI with staged assets).
$env:RAPID_OCR_REQUIRE_EXTERNAL_ASSETS = "1"
$env:RAPID_OCR_MODEL_ROOT = "<workspace>/OCR-Model"
$env:RAPID_OCR_FORMULA_TEST_ROOT = "<workspace>/Formula-TestSet"
cargo test --all-targets
```

| variable | purpose |
| --- | --- |
| `RAPID_OCR_MODEL_ROOT` | OCR model root (`<workspace>/OCR-Model`); also locates the formula model, the formula detector, and (via its parent) `OCR-test-image` |
| `RAPID_OCR_FORMULA_MODEL` | direct path to `pp_formulanet_plus_m.onnx` |
| `RAPID_OCR_FORMULA_DETECT_MODEL` | direct path to `pix2text-mfd-1.5.onnx` |
| `RAPID_OCR_FORMULA_TEST_ROOT` | formula test-set root (`<workspace>/Formula-TestSet`) |
| `RAPID_OCR_TEST_IMAGES` | page images used by the page-level formula integration tests |
| `RAPID_OCR_REQUIRE_EXTERNAL_ASSETS` | `1` turns a missing asset into a test failure |

### Quality gates

```text
cargo test --all-targets
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo check --features directml-provider,cuda-provider,cann-provider
```

`--all-features` additionally enables `opencv-backend`; that build requires an
OpenCV installation and is not part of the checks above.

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

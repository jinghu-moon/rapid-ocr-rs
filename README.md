# rapid-ocr-rs

`rapid-ocr-rs` is a reusable Rust OCR crate for PaddleOCR-family ONNX models.
It owns image normalization, detection, text-region cropping, recognition,
CTC decoding, polygon output, model resolution, and ONNX Runtime sessions. It
does not depend on Tauri, SQLite, a clipboard implementation, or a specific
application.

## Platform support

**The only supported platform is Windows x64 with the MSVC ABI
(`x86_64-pc-windows-msvc`).** Other targets fail at compile time with a single explicit
`compile_error!` from `src/platform_gate.rs` instead of a scatter of "missing Windows
API" diagnostics; the gate itself is verifiable with
`pwsh -NoProfile -File tools/check_platform_gate.ps1`.

Explicit non-goals: **Windows x86 (i686)**, **Windows ARM64**, **the Windows GNU ABI**,
and also Wine, WSL, Linux and macOS. This is a deliberate narrowing, not an unfinished
port. 32-bit Windows x86 is a real limitation rather than a `cfg` change: the
precompiled ONNX Runtime, the DirectML/CUDA execution-provider DLLs and the ~566 MB
formula recognition model are all x64 artifacts, and the crate itself is only built and
verified on the MSVC ABI (import library names, `#[link(name = "psapi")]`, the ort
prebuilt runtime). Windows ARM64 has the same problem in a different shape — the whole
native stack would have to be re-collected and re-measured. A future port gets its own
platform plan rather than incremental `cfg` additions here.

Platform differences are expected to live only in platform implementation modules
(currently `src/runtime/memory.rs`); business code must not branch on the OS.

## License

The `rapid-ocr-rs` source code is licensed under the Apache License, Version
2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE). Third-party dependencies,
ONNX Runtime binaries, OCR model weights, and model dictionaries have separate
license and redistribution terms; see [THIRD_PARTY_NOTES.md](THIRD_PARTY_NOTES.md)
before redistributing a complete application or model bundle.

## Current baseline

- PP-OCRv6 detector + recognizer; direction classifier disabled unless a
  compatible classifier artifact is explicitly configured.
- CPU is the default provider. DirectML and CUDA are opt-in Cargo features and
  must be benchmarked on the target machine; CANN is not a Windows target and has
  been removed.
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
   guarantee, not post-hoc filtering. Whitening is **per region**, not a
   per-text-box overlap rule: there is no text-detection-coverage threshold, and
   `FormulaPolicy` deliberately exposes no such knob (an earlier draft documented
   a `text_overlap_skip_ratio` that never affected behavior; it was removed
   rather than left as a fake API).
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
- Whitening changes the pixels the text detector sees, so text regions **outside**
  the formula region can be re-segmented. Measured on `08数字公式与符号.png`:
  whitening one row merges/splits a distant row (`42.7 ms` becomes `42.7` + `ms`).
  Content is not lost, but region boundaries and line grouping can change.
  Callers that require byte-stable text segmentation should not enable formula
  routing.
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

The single model inventory is `ModelSet` (`src/model_set.rs`): one entry per file
with its `role`, expected SHA-256 and size. `validate_model_files` returns the
state of **every** file (`missing` / `present` / `corrupt`) in one pass, and
`ModelManifest::validate_files` is a thin wrapper over it that fails on the first
file that is not `present`. A set is `complete` only when every file is present
**and** every file carries a hash, so an unhashed entry can never report a
complete set. File names must be bare relative names: absolute paths, path
separators and `..` are rejected by one shared rule.

There is exactly **one** authoritative source per model directory
(`src/model_source.rs`): if `<model-dir>/manifest.json` exists it is the only
source and `assets/default_models.yaml` is not consulted at all; otherwise the
default table is the only source. There is no merge and no optional override. A
manifest must declare `schema_version: 1` and a `files` array; the legacy
four-field shape (`detector`/`recognizer`/`dictionary`/`classifier`) and any other
`schema_version` produce a locating error. If the selected source lacks a role
the requested pipeline needs, the error lists the missing roles instead of
degrading silently.

Keep model weights outside the crate source tree and record their upstream URL,
revision, checksum, and redistribution terms in the consuming application's
manifest (see `assets/manifest.example.json`).

### What the digest cache does — and does not — guarantee

Every per-file state question (`/api/models`, the formula admission check, and the
two formula load paths) is answered through one process-wide identity-keyed cache
(`src/model_verify.rs`). A file's **identity** is its path, size, mtime **and the
SHA-256 of its first and last 64 KiB**; the cached value is the full SHA-256 that
was actually computed for that identity. A hit costs one `stat` plus one 128 KiB
read, independent of file size; a full re-hash happens on first sight, and again
whenever the size, the content windows, or (for reporting) the mtime change. For
files of 128 KiB or less the two windows overlap and cover the whole content.

The promise is deliberately narrow and exact:

> content is verified at first use and re-verified whenever the file's identity
> changes; the service does **not** claim that the on-disk content is trusted at
> all times.

The residual blind spot is a same-size, same-mtime edit that also keeps the first
and last 64 KiB byte-identical (edit the middle only). The partial digest is a
**heuristic that narrows the window, not a security boundary** — an attacker who
can write the file can also preserve its head and tail. The deterministic escape
hatches are `--reverify-models` at startup (cold-verifies every file the run will
load and refuses to start when one is missing or corrupt) and
`POST /api/models/reverify` while running (clears the cache, cold-verifies, and
rebuilds the engine session so a replaced file cannot keep being served from
memory).

### One run model plan

`rapidocr serve` resolves **one** list of "files this run will load" at startup and
everything reads it: `--reverify-models`, `POST /api/models/reverify`, the
`pipelines` block of `/api/models`, admission on both queues, and the two loading
paths (`pin_engine_paths` and `FormulaPolicy`).

- Text pipeline: `detector` + `recognizer` + `dictionary`, plus `classifier` when
  `use_cls` is on.
- Formula pipeline: **only when formula routing is enabled** (a detector is
  configured through `--formula-detector`, or declared by the model set as the
  `formula_detector` role) — `formula_recognizer` **and** `formula_detector`.
- Formula routing disabled ⇒ the formula models are not in the plan at all: neither
  entry point reads the 566 MB recognizer, and a corrupt one neither blocks startup
  nor ordinary OCR.

The two semantics are compatible and both hold:

- `--reverify-models` is an explicit opt-in meaning "verify this run's whole plan at
  startup and refuse to start if any file in it is unusable". With formula enabled a
  corrupt formula model (detector or recognizer) fails startup, and the error names
  the file and which pipeline it belongs to.
- Runtime admission stays scoped per pipeline: a corrupt formula model yields a 409
  with `detail.scope = "formula"` on the formula queue while ordinary OCR keeps
  working. `POST /api/models/reverify` reports per pipeline — the text engine outcome
  (`pipelines.text.outcome`) and the formula result plus its routing state
  (`pipelines.formula.*`) — so "text ready, formula corrupt" is explicit instead of a
  bare `ready`.

### What "verified" means without a declared digest

An external `--formula-detector` that no model set declares has **no trusted digest**.
"Verified" then means exactly three things: the file exists, it is readable, and it
looks like an ONNX `ModelProto` (its protobuf prologue starts with the `ir_version`
field: tag `0x08` followed by a varint in `1..=64`; an empty file is rejected). This
is a heuristic, not an integrity proof: the service reports `sha256: null` for such a
file and never claims its content was checked. The cold verification still computes
and reports the actual digest it read. When a model set does declare a digest, that
declared value is always enforced by hash.

### Flow logging: following one upload end to end

`rapidocr serve` writes **no** flow log by default. Turn it on with `--log-level flow`
or the equivalent environment variable:

```text
rapidocr serve --model-dir <DIR> --log-level flow
RAPID_OCR_SERVE_LOG=flow rapidocr serve --model-dir <DIR>     # same switch, CLI wins
```

Every HTTP request then gets one line (request id, method, path, status, response
bytes, duration) and every job gets lifecycle lines (id, queue, the admission
decision **and its reason**, queue position, wait time, run time, terminal state,
result size; on failure the status code plus `code` and `detail`). Both kinds share
**the same request id**, so one upload can be followed from the request line to the
terminal line:

```text
serve-flow: req=7 POST /api/ocr -> 202 bytes=334 in 6ms
serve-flow: req=7 job=job-0000000000000003 queue=text admission=accepted decision=run queued position=0
serve-flow: req=7 job=job-0000000000000003 queue=text running wait_ms=3
serve-flow: req=7 job=job-0000000000000003 queue=text terminal state=succeeded result_bytes=35201 backend_ms=881
```

A refused upload has no job id — the request id is its only correlation key, which is
exactly what is needed when "the upload did nothing" has to be explained:

```text
serve-flow: req=9 queue=formula admission=rejected status=409 code=models_corrupt detail={...}
```

An unknown level (`--log-level chatty`) refuses to start and names the switch, rather
than silently behaving as `off`. The startup banner always prints the effective level
and the current value of `RAPID_OCR_SERVE_LOG` (`<unset>` when unset).

The page has a matching switch: **`?verbose=1`** (or `?verbose=0` to turn it off, or
`Ctrl+Alt+L`; the choice is remembered in `localStorage`). The diagnostics panel then
lists the most recent requests with their method, URL, HTTP status, duration and error
code, **including the response body of failures**. With verbose off, that section still
appears whenever the last request failed — so "why did nothing render?" is answerable
without opening devtools.

### The `/result` contract is pinned from both sides

The inline page parses `/api/jobs/{id}/result` against a frozen field set and refuses
to render anything that does not match it (`regions[]`, `kind`, `polygon.points` as
exactly four `[x, y]` pairs, `recognition.text`, `recognition.score`, `formula.latex`,
`plain_text`, `timings`, `timing_ledger`). A renamed or missing field is reported with
its exact path and never silently tolerated. Four independent checks keep the two sides
from drifting apart:

| Check | Command / test | What it pins |
| --- | --- | --- |
| served body | `cargo test --features serve the_served_result_carries_every_path_the_page_reads` | the real endpoint's `/result` satisfies every path the page reads, including the exact polygon nesting |
| page source | `cargo test --features serve the_page_reads_exactly_the_pinned_result_paths` | the page still contains those exact accessor expressions, still sends `?queue=`, and re-introduces no alias |
| captured bytes | `cargo test --features serve the_captured_real_result_body_still_satisfies_the_page_contract` | a real captured 42-region body (SHA-256 pinned) still satisfies the same contract |
| page's own parser | `node tools/check-page-result-contract.mjs` | runs the page's own `normalizeResult` over that captured body in node (42 regions must render) and requires nine field-rename mutations to be rejected **by path** |

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
stable sampling manifest (a stable hash of the dataset-relative path plus the
ground truth — **not** "first N", and not the image bytes), per-sample
records, failure classification, exact/normalized match, CER, EOS/truncated
counts, throughput with P50/P95, peak working set, and provider/thread settings.

Sampling is a stable hash of **dataset-relative path + ground truth**: the same
dataset selects the same samples on any machine and under any absolute root, and
`SampleStrategy::Hash` never degenerates into "first N". Image *content* is
deliberately **not** part of the sampling key — it is recorded separately as
`content_sha256`, so a data change is reported as a content change instead of
silently re-selecting a different subset.

Manifests carry three digests:

| field | covers | changes when |
| --- | --- | --- |
| `manifest_sha256` | dataset/split/subset/strategy/limit + entries **in evaluation order** | the sample set *or* the evaluation order changes |
| `sample_set_sha256` | the same, **sorted by path** (order-independent) | the set of samples or their labels changes |
| `content_sha256` | the above plus every image file's SHA-256 (order-independent, `null` if any image is unreadable) | an image file's bytes change |

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
reports the difference instead of silently evaluating a different subset. It also
verifies the image-content digest when the recorded manifest has one. Add
`--manifest-only` to do this check without loading the model (see "Verification
tiers").

Long datasets can be split across processes and merged back:

```powershell
# 5 shards, each evaluating every 5th manifest entry.
foreach ($i in 0..4) {
  Start-Process cargo -ArgumentList @('run','--release','--bin','formula_eval','--',
    '--model',$Model,'--dataset-root',$TestSet,'--dataset','unimer','--subset','cpe',
    '--shard',"$i/5",'--expect-manifest',"target/formula-eval/manifest-unimer-cpe.json",
    '--output',"target/formula-eval/unimer-cpe.shard$i.json")
}
# Merge reorders records by manifest and recomputes metrics with the same summarizer.
cargo run --release --bin formula_eval -- --merge-shards target/formula-eval/unimer-cpe.shard*.json `
  --output target/formula-eval/unimer-cpe.json
```

The merge refuses to combine reports whose `model.sha256`, provider, `batch_size`,
dataset/split/subset, manifest, image content digest, or Python reference digest
disagree, and it aggregates a Python comparison across all shards rather than
keeping the first shard's partial counts.

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
(`windows:GetProcessMemoryInfo.PeakWorkingSetSize` — the only memory口径 in the
crate; a failed sample is reported with its Win32 error instead of being omitted). Formula throughput is always reported
separately from ordinary OCR; `--ocr-baseline <bench.json>` only adds the
ordinary OCR benchmark side by side and never merges the two into one number.

`deterministic_tokens` compares each batch row against the same image decoded
alone. The comparison uses the tokens **up to and including EOS**, because the
in-graph `Loop` pads every row of a batch to the same width: comparing whole
rows would report a difference that is only padding. Measured batch=1/2/4 over
real val images are identical by that definition.

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
cargo check --features directml-provider,cuda-provider
```

### Verification tiers: what to run, and when

Full-dataset evaluation is **not** a routine regression step. A complete run over
`val` + `im2latex` + all four UniMER subsets is ~40,000 images and takes hours
(UniMER-CPE alone is ~3 h with sharding, because its mean label is 658 characters
and the in-graph autoregressive `Loop` dominates). Run the cheapest tier that can
actually falsify your change.

| Change | Required verification | Cost |
| --- | --- | --- |
| Ordinary code, docs, refactors with no formula behavior change | `cargo test --all-targets` (fixtures only, no external assets) + `cargo fmt`/`clippy` | seconds |
| Formula routing, outputs, input limits, detector decoding | above + `cargo test --all-targets` with `RAPID_OCR_MODEL_ROOT`/`RAPID_OCR_TEST_IMAGES` set (page-level integration tests) + one real page through the CLI | ~1 min |
| Tokenizer, post-processing, model invocation, batch/EOS logic | above + **100-image im2latex smoke** + **501-image `val` with the Rust/Python link comparison** | ~15 min |
| Model file, preprocessing, decode/EOS semantics, metric implementation | above + the affected **full dataset(s)**; re-record the baseline | hours |
| Release acceptance | full datasets + `formula_bench` on every available provider, serially and uncontended | hours |

Commands for each tier are in "Formula benchmark and evaluation" above. The
committed baseline `tests/baseline/formula-evaluation-2026-10-03.json` may be
**reused** instead of re-running a full dataset only while all of the following
are unchanged: the model file, image preprocessing, the tokenizer, post-processing,
batch inference and EOS handling, and the metric implementation. If any of them
changes, the stored numbers no longer describe the current code and the affected
full run must be repeated.

Two properties make the cheap tiers meaningful:

- `--manifest-only` resolves the dataset and writes the manifest (including
  image content hashes) **without loading the model**, so a data-integrity check
  costs seconds instead of hours:
  `formula_eval --manifest-only --dataset-root $TestSet --dataset latexocr --split validate --expect-manifest <manifest.json>`
- `--expect-manifest` verifies the sample-selection digest (`manifest_sha256`)
  and the image-content digest (`content_sha256`), so replacing an image file
  without changing its path or label is detected. `sample_set_sha256`
  distinguishes "a different set of samples" from "the same samples in a
  different order". Manifests recorded before `content_sha256` existed still pin
  the sample set, and `--expect-manifest` says so explicitly instead of
  pretending to verify content.

Long datasets can be sharded across processes when a full run is genuinely
required (`--shard INDEX/COUNT` + `--merge-shards`); the manifest still covers the
whole set, the merge reuses the same summarizer, and it refuses to merge reports
from different models, providers, batch sizes, or Python references.

## Provider feature verification

Provider features are opt-in and can be compile-checked or tested on a machine
without a GPU:

```text
cargo check --features directml-provider
cargo check --features cuda-provider
cargo check --features directml-provider,cuda-provider
cargo test --features directml-provider
cargo test --features cuda-provider
```

## Windows x64 acceptance

The stage-by-stage evidence for the Windows x64 (MSVC) narrowing lives in
`docs/04-windows-phase-reports.md`; the plan is
`docs/03-windows-only-optimization-tasks.md` (file name kept from the first round).
Summary of the final state:

- 12-image OCR gates are bit-identical to the pre-refactor baseline
  (mean CER `0.44765135645866394`, region average `34.8333`), release binaries are
  3.7% smaller, and the im2latex-100 formula smoke reproduces exactly.
- ONNX Runtime inference is about 85.6% of page time and the named Rust side about
  13.7%, **with a stated residual of -6.75 ms/page (0.69%)**: the timing ledger is a
  diagnostic instrument whose named components are measured over different (sequential)
  ranges than `total_ms` — the outer `OcrTimings::preprocess_ms` window ends before
  `inner.run()` is entered, so no wall-clock interval is counted twice — and it
  therefore does not form a strict partition of `total_ms`. It is not the acceptance
  basis for any performance claim. The stage-6 conclusion (the bottleneck is ONNX
  Runtime, not Rust hot paths) rests on the inference share being an order of magnitude
  larger than every Rust component, which a residual of that size cannot overturn. See
  `docs/04-windows-phase-reports.md`.
- No speedup is claimed anywhere: the thread matrix, the `-C target-cpu=x86-64-v3`
  A/B (-2.84% median over 5 interleaved pairs, one pair +19.35%) and the formula
  batch curve all land inside this machine's run-to-run noise, which reaches 29-39%
  between identical binaries. Any future performance claim must come from an
  interleaved A/B on the target machine.
- Acceleration claims require measurement: `tools/check_provider_claims.ps1` fails
  any report that claims a non-CPU provider without fallback while its measured p50
  is within 10% of CPU. On this machine DirectML passes (1.99x) and CUDA fails
  (1.00x, cuDNN missing), which is the documented finding rather than a tool bug.
## Implementation reference

The pipeline behavior is cross-checked against RapidOCR and the local
`paddle-ocr-rs` reference. Their licenses and model licenses remain separate
from this crate's Apache-2.0 license; see `THIRD_PARTY_NOTES.md`.

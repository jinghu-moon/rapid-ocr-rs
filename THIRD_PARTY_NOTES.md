# Third-party and model notes

`rapid-ocr-rs` is Apache-2.0. The initial pipeline was derived from the local
`refer/paddle-ocr-rs-main` reference and keeps its Apache-2.0 notice. The
repository-level attribution is in `NOTICE`.

The following are project references or implementation influences, not a
claim that their model artifacts are licensed under this crate's license:

- `paddle-ocr-rs`: Apache-2.0 source reference.
- RapidOCR: Apache-2.0 source and deployment reference.
- PaddleOCR: upstream model and pipeline project; review its code and model
  terms separately for anything redistributed with an application.
- RapidDoc: Apache-2.0 source reference for PP-FormulaNet_plus preprocessing,
  tokenizer metadata handling, and LaTeX post-processing semantics.
- PP-FormulaNet / PaddleOCR / UniMER / im2latex: upstream model and dataset
  projects. Their weights, dictionaries, tokenizer metadata, and test images
  retain their own licenses and are not relicensed by this crate.

## PP-FormulaNet_plus-M model

- ModelScope URL:
  `https://www.modelscope.cn/models/RapidAI/RapidDoc/resolve/v1.0.0/formula/PP-FormulaNet_plus-M/pp_formulanet_plus_m.onnx`
- Version: RapidDoc `v1.0.0`
- SHA-256:
  `71b6d389cf7b857e45252a4b98cfced1a3ffca7bf24d9497d02d052a41d9493b`
- On-disk size: 593,915,961 bytes (about 594 MB).
- ONNX contract: input `x` `FLOAT32 [N, 1, 384, 384]`, output `fetch_name_0`
  `INT64 [N, L]`, IR 10, opset 18, in-graph `Loop` whose maximum output width is
  2561 columns.
- Tokenizer: the model's `character` metadata embeds `fast_tokenizer_file`
  (vocab 50,000; `<s>`/`<pad>`/`</s>`/`<unk>` = 0/1/2/3) and
  `tokenizer_config_file`.
- License: the RapidDoc repository and ModelScope model page identify
  Apache-2.0; verify the current upstream page before redistribution.
- The crate does not bundle the ~594 MB ONNX model. Applications must download
  it separately, verify the SHA-256, and accept the upstream terms.

## Pix2Text-MFD-1.5 page formula detector (optional)

Used only when `FormulaPolicy::detector_path` is set.

- File: `OCR-Model/Formula-Detection-Model/pix2text-mfd-1.5.onnx`
  (76.6 MB, not bundled).
- Source: Pix2Text / P2T (breezedeus) model card for `Pix2Text-MFD-1.5`.
- ONNX contract: input `images` `FLOAT32 [batch, 3, height, width]`, output
  `output0` `FLOAT32 [batch, 6, A]` (4 box channels + 2 class scores), IR 9,
  opset 19, metadata `imgsz=[768, 768]`, `stride=32`,
  `names={0: 'embedding', 1: 'isolated'}`.
- **License conflict that must be resolved by the consumer**: the model card
  front-matter declares `license: mit`, while the ONNX metadata retains
  `license: AGPL-3.0 License (https://ultralytics.com/license)` and
  `description: Ultralytics YOLO11m model`. These two statements are not
  compatible; do not redistribute this model until the upstream terms have been
  confirmed for the intended use.
- The crate does not bundle the detector. Detection accuracy on non-formula
  pages is limited; see the README's formula limitations section.

## Test fixtures committed to this repository

The following small fixtures are committed so that a clean clone can run the
contract and equivalence tests without any external asset:

- `tests/fixtures/formula-onnx/*.onnx` (KB-scale synthetic ONNX graphs, rebuilt
  by `tools/build_formula_onnx_fixtures.py`);
- `tests/fixtures/ocr-onnx/*.onnx`;
- `tests/fixtures/formula-golden/*` (PNG inputs + NumPy golden tensors);
- `tests/fixtures/formula-tokenizer/fast_tokenizer.json` (extracted from the
  PP-FormulaNet_plus-M `character` metadata) and `cases.json`;
- `tests/fixtures/formula-detect/*` (synthetic page images + golden boxes,
  rebuilt by `tools/formula_detect_reference.py`);
- `tests/fixtures/formula-postprocess/cases.json` (generated with the real
  `ftfy` by `tools/formula_ftfy_reference.py`).

`.gitignore` excludes all other `*.onnx`, model caches, dataset copies, and
evaluation/benchmark outputs, and the exceptions are limited to
`tests/fixtures/**/*.onnx`.

`unicode-normalization` (MIT/Apache-2.0) is the only dependency added for the
`ftfy` port; all `ftfy` tables are generated from the installed Python package
rather than transcribed by hand.

Runtime and image dependencies are consumed through Cargo. The important
runtime boundary is ONNX Runtime (`ort`/`ort-sys`), whose native DLL must be
distributed separately according to the selected feature and target.

Cargo dependencies retain their own licenses. A release bundle should produce
a dependency license inventory from `Cargo.lock` (for example with
`cargo-about`) rather than treating this crate's Apache-2.0 license as a
license for transitive dependencies.

The PP-OCRv6 detector, recognizer, and dictionary are model artifacts, not
source code. Their upstream URL, exact revision, SHA-256, and redistribution
terms must be recorded in the application model manifest before packaging.
The crate verifies SHA-256 for every artifact (weights **and** recognition
dictionaries) through the shared per-file validator `model_set::validate_model_files`,
which `ModelManifest::validate_files` wraps; it does not invent or embed
checksums for artifacts that are not present.

No cloud OCR service is used. Model downloads, when enabled by the application,
must use HTTPS and an explicit expected SHA-256.

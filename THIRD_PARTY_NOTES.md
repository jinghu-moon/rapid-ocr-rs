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
- License: the RapidDoc repository and ModelScope model page identify
  Apache-2.0; verify the current upstream page before redistribution.
- The crate does not bundle the ~594 MB ONNX model. Applications must download
  it separately, verify the SHA-256, and accept the upstream terms.

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
The crate verifies SHA-256 for every artifact through `ModelManifest::validate_files`;
it does not invent or embed checksums for weights that are not present.

No cloud OCR service is used. Model downloads, when enabled by the application,
must use HTTPS and an explicit expected SHA-256.

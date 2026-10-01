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

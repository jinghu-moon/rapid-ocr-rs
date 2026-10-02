#!/usr/bin/env python3
"""Independent reference for the page-level formula detector `pix2text-mfd-1.5`.

This script is the *independent* implementation that `src/formula/detect.rs` is checked
against.  It uses only numpy + onnxruntime + Pillow and deliberately never imports the
`ultralytics` package (which is not installed here); the Ultralytics inference steps
(letterbox -> raw YOLO11 detect head output -> `scale_boxes` -> class-aware NMS) are
re-implemented from the published algorithm so that agreement with the Rust port is
real evidence and not a shared dependency.

It also *generates* the committed fixtures under `tests/fixtures/formula-detect/`:

    page.png         synthetic 768x384 page holding `formula-golden/formula_text.png`
    page_scaled.png  synthetic 1152x576 page (letterbox scale 2/3, exercises scale-back)
    golden.json      geometry + raw head boxes + final polygons for page.png
    golden_scaled.json  same for page_scaled.png

Model location: `--model`, else `RAPID_OCR_FORMULA_DETECT_MODEL`, else
`RAPID_OCR_MODEL_ROOT` + the standard relative path.  Missing model => hard failure
with a clear message (never a silent skip).
"""

from __future__ import annotations

import argparse
import ast
import json
import os
import sys
from pathlib import Path

import numpy as np
import onnxruntime as ort
from PIL import Image

# --------------------------------------------------------------------------------------
# Ultralytics inference constants (must match src/formula/detect.rs)
# --------------------------------------------------------------------------------------
INPUT_SIZE = 768
CONFIDENCE_THRESHOLD = 0.25
IOU_THRESHOLD = 0.7
MAX_DETECTIONS = 300
PAD_VALUE = 114
CLASS_OFFSET = 7680.0  # Ultralytics `max_wh`, used to make NMS class-aware

MODEL_RELATIVE = "Formula-Detection-Model/pix2text-mfd-1.5.onnx"
REPO_ROOT = Path(__file__).resolve().parents[1]
FIXTURE_DIR = REPO_ROOT / "tests/fixtures/formula-detect"
SOURCE_IMAGE = REPO_ROOT / "tests/fixtures/formula-golden/formula_text.png"

# (canvas_w, canvas_h, paste_x, paste_y, formula_scale)
#
# The formula must appear at roughly the size it has in the detector's training data,
# otherwise the `isolated` head stays far below any usable confidence: pasting
# `formula_text.png` at its native 420x160 onto a 768x384 page yields max score
# ~0.0015, i.e. *no* detection at all, while a 1.7x paste of the same page yields
# ~0.71.  See README.md in the fixture directory.
PAGE_SPECS = (
    ("page.png", "golden.json", 768, 384, 40, 48, 1.7),
    ("page_scaled.png", "golden_scaled.json", 1152, 576, 40, 84, 2.55),
)


def resolve_model(explicit: str | None) -> Path:
    candidates = []
    if explicit:
        candidates.append(Path(explicit))
    env_direct = os.environ.get("RAPID_OCR_FORMULA_DETECT_MODEL")
    if env_direct:
        candidates.append(Path(env_direct.strip().strip('"')))
    env_root = os.environ.get("RAPID_OCR_MODEL_ROOT")
    if env_root:
        root = Path(env_root.strip().strip('"'))
        candidates.append(root if root.is_file() else root / MODEL_RELATIVE)
    for candidate in candidates:
        if candidate.is_file():
            return candidate
    raise SystemExit(
        "pix2text-mfd-1.5.onnx not found. Pass --model <path>, or set "
        "RAPID_OCR_FORMULA_DETECT_MODEL to the .onnx file, or set "
        f"RAPID_OCR_MODEL_ROOT to the model root containing {MODEL_RELATIVE}."
    )


def parse_names(raw: str | None) -> list[str]:
    """Parse the Ultralytics `names` metadata, e.g. `{0: 'embedding', 1: 'isolated'}`."""
    if not raw:
        raise SystemExit("model metadata has no `names` entry; refusing to guess classes")
    parsed = ast.literal_eval(raw)
    if not isinstance(parsed, dict):
        raise SystemExit(f"unexpected `names` metadata: {raw!r}")
    return [str(parsed[key]) for key in sorted(parsed)]


def compose_page(width: int, height: int, paste_x: int, paste_y: int, scale: float):
    """White RGB canvas of `width`x`height` with the formula pasted at a known rect."""
    formula = Image.open(SOURCE_IMAGE).convert("RGB")
    target = (round(formula.width * scale), round(formula.height * scale))
    resized = formula.resize(target, Image.BILINEAR)
    if paste_x + target[0] > width or paste_y + target[1] > height:
        raise SystemExit(
            f"pasted formula {target} at ({paste_x},{paste_y}) does not fit in {width}x{height}"
        )
    canvas = Image.new("RGB", (width, height), (255, 255, 255))
    canvas.paste(resized, (paste_x, paste_y))
    rect = [paste_x, paste_y, paste_x + target[0], paste_y + target[1]]
    return canvas, rect, list(target)


def letterbox(image: Image.Image, size: int = INPUT_SIZE):
    """Ultralytics LetterBox: keep aspect ratio, pad with 114, centred."""
    width, height = image.size
    ratio = min(size / height, size / width)
    new_w, new_h = round(width * ratio), round(height * ratio)
    resized = image.resize((new_w, new_h), Image.BILINEAR)
    canvas = Image.new("RGB", (size, size), (PAD_VALUE, PAD_VALUE, PAD_VALUE))
    dw, dh = (size - new_w) / 2, (size - new_h) / 2
    left, top = round(dw - 0.1), round(dh - 0.1)
    canvas.paste(resized, (left, top))
    # RGB stays RGB: Ultralytics converts the BGR cv2 frame *to* RGB before the model.
    array = np.asarray(canvas, dtype=np.float32) / 255.0
    tensor = np.ascontiguousarray(array.transpose(2, 0, 1)[None])
    meta = {
        "scale": float(ratio),
        "left": int(left),
        "top": int(top),
        "new_size": [int(new_w), int(new_h)],
    }
    return tensor, meta


def decode_raw(output: np.ndarray, confidence_threshold: float):
    """`output0` is [1, 6, A] -> per-anchor [cx, cy, w, h, score_cls0, score_cls1].

    The exported graph already contains DFL/dist2bbox/sigmoid, so `cx`/`cy` are
    absolute letterboxed-input pixels and NO stride/anchor offset may be re-added.
    """
    channels, anchors = output.shape[1], output.shape[2]
    nc = channels - 4
    records = []
    for index in range(anchors):
        scores = output[0, 4 : 4 + nc, index]
        best = int(np.argmax(scores))
        score = float(scores[best])
        if score <= confidence_threshold:
            continue
        cx, cy, w, h = (float(value) for value in output[0, 0:4, index])
        records.append(
            {
                "xyxy": [cx - w / 2, cy - h / 2, cx + w / 2, cy + h / 2],
                "score": score,
                "class_id": best,
            }
        )
    return records


def scale_back(record, meta, original_size):
    """Ultralytics `scale_boxes` back to original image pixels."""
    width, height = original_size
    x1, y1, x2, y2 = record["xyxy"]
    x1 = min(max((x1 - meta["left"]) / meta["scale"], 0.0), width)
    x2 = min(max((x2 - meta["left"]) / meta["scale"], 0.0), width)
    y1 = min(max((y1 - meta["top"]) / meta["scale"], 0.0), height)
    y2 = min(max((y2 - meta["top"]) / meta["scale"], 0.0), height)
    return {
        "xyxy": [min(x1, x2), min(y1, y2), max(x1, x2), max(y1, y2)],
        "score": record["score"],
        "class_id": record["class_id"],
    }


def iou(a, b) -> float:
    x1, y1 = max(a[0], b[0]), max(a[1], b[1])
    x2, y2 = min(a[2], b[2]), min(a[3], b[3])
    inter = max(0.0, x2 - x1) * max(0.0, y2 - y1)
    area_a = max(0.0, a[2] - a[0]) * max(0.0, a[3] - a[1])
    area_b = max(0.0, b[2] - b[0]) * max(0.0, b[3] - b[1])
    union = area_a + area_b - inter
    return inter / union if union > 0 else 0.0


def nms(records, iou_threshold: float, max_detections: int):
    """Class-aware greedy NMS, offset boxes per class like Ultralytics (`c = cls * 7680`)."""
    ordered = sorted(records, key=lambda record: record["score"], reverse=True)
    keep = []
    for candidate in ordered:
        offset = candidate["class_id"] * CLASS_OFFSET
        candidate_box = [value + offset for value in candidate["xyxy"]]
        suppressed = False
        for kept in keep:
            kept_offset = kept["class_id"] * CLASS_OFFSET
            kept_box = [value + kept_offset for value in kept["xyxy"]]
            if iou(candidate_box, kept_box) > iou_threshold:
                suppressed = True
                break
        if not suppressed:
            keep.append(candidate)
            if len(keep) >= max_detections:
                break
    return keep


def polygon(record):
    x1, y1, x2, y2 = record["xyxy"]
    return [[x1, y1], [x2, y1], [x2, y2], [x1, y2]]


def detect(session, image: Image.Image):
    tensor, meta = letterbox(image)
    output = session.run(None, {session.get_inputs()[0].name: tensor})[0]
    raw = decode_raw(output, CONFIDENCE_THRESHOLD)
    scaled = [scale_back(record, meta, image.size) for record in raw]
    final = nms(scaled, IOU_THRESHOLD, MAX_DETECTIONS)
    return meta, raw, scaled, final, int(output.shape[2])


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--model", default=None, help="path to pix2text-mfd-1.5.onnx")
    parser.add_argument("--fixtures-dir", default=str(FIXTURE_DIR))
    parser.add_argument("--quiet", action="store_true")
    args = parser.parse_args()

    model_path = resolve_model(args.model)
    fixtures_dir = Path(args.fixtures_dir)
    fixtures_dir.mkdir(parents=True, exist_ok=True)

    session = ort.InferenceSession(str(model_path), providers=["CPUExecutionProvider"])
    names = parse_names(session.get_modelmeta().custom_metadata_map.get("names"))
    output_shape = session.get_outputs()[0].shape
    print(f"model      : {model_path}")
    print(f"classes    : {names}")
    print(f"output     : {output_shape}")

    for page_name, golden_name, width, height, paste_x, paste_y, scale in PAGE_SPECS:
        page, rect, pasted_size = compose_page(width, height, paste_x, paste_y, scale)
        page.save(fixtures_dir / page_name)

        meta, raw, scaled, final, anchors = detect(session, page)
        expected_anchors = sum((INPUT_SIZE // stride) ** 2 for stride in (8, 16, 32))
        if anchors != expected_anchors:
            raise SystemExit(
                f"{page_name}: model returned {anchors} anchors, expected {expected_anchors} "
                f"(stride-major {INPUT_SIZE // 8}^2 + {INPUT_SIZE // 16}^2 + {INPUT_SIZE // 32}^2)"
            )
        print(f"\n=== {page_name} ===")
        print(f"canvas          : {width}x{height}   formula pasted {(scale)}x -> {pasted_size}")
        print(f"pasted_rect     : {rect}")
        print(f"letterbox       : {meta}  anchors={meta['new_size']} ")
        print(f"anchor count    : fed {INPUT_SIZE}x{INPUT_SIZE} -> {expected_anchors} (measured)")
        print(f"raw (>conf)     : {len(raw)}   after NMS: {len(final)}")
        for record in final:
            x1, y1, x2, y2 = record["xyxy"]
            print(
                f"  {names[record['class_id']]:<10s} score={record['score']:.6f} "
                f"xyxy=({x1:.2f},{y1:.2f},{x2:.2f},{y2:.2f}) "
                f"iou(pasted_rect)={iou(record['xyxy'], rect):.4f}"
            )
        if not final:
            raise SystemExit(f"{page_name}: reference found no box above confidence; fixture is useless")

        # 坐标约定自检：导出的图直接给出绝对像素中心，不能再叠加 anchor 偏移。若叠加了，
        # 检测框中心会偏离我们贴图的位置，这里必须立刻失败。
        x1, y1, x2, y2 = final[0]["xyxy"]
        centre = ((x1 + x2) / 2, (y1 + y2) / 2)
        rect_centre = ((rect[0] + rect[2]) / 2, (rect[1] + rect[3]) / 2)
        offset = (abs(centre[0] - rect_centre[0]), abs(centre[1] - rect_centre[1]))
        print(
            f"centre check    : detected ({centre[0]:.2f},{centre[1]:.2f}) vs "
            f"pasted_rect centre ({rect_centre[0]:.2f},{rect_centre[1]:.2f}) "
            f"-> offset ({offset[0]:.2f},{offset[1]:.2f}) px"
        )
        if offset[0] > 0.25 * (rect[2] - rect[0]) or offset[1] > 0.25 * (rect[3] - rect[1]):
            raise SystemExit(
                f"{page_name}: detected centre is far from the pasted rect; the coordinate "
                "convention (absolute letterboxed pixels vs anchor offsets) is wrong"
            )

        golden = {
            "image": page_name,
            "image_size": [width, height],
            "pasted_rect": rect,
            "paste_scale": scale,
            "source_image": "tests/fixtures/formula-golden/formula_text.png",
            "input_size": INPUT_SIZE,
            "anchor_count": anchors,
            "confidence_threshold": CONFIDENCE_THRESHOLD,
            "iou_threshold": IOU_THRESHOLD,
            "max_detections": MAX_DETECTIONS,
            "class_names": names,
            "letterbox": meta,
            "raw_boxes": [
                {
                    "xyxy": [round(value, 6) for value in record["xyxy"]],
                    "score": round(record["score"], 6),
                    "class_id": record["class_id"],
                }
                for record in raw
            ],
            "raw_box_count": len(raw),
            "boxes": [
                {
                    "polygon": [[round(x, 6), round(y, 6)] for x, y in polygon(record)],
                    "score": round(record["score"], 6),
                    "class_id": record["class_id"],
                    "class_name": names[record["class_id"]],
                }
                for record in final
            ],
        }
        (fixtures_dir / golden_name).write_text(
            json.dumps(golden, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
        )
        if not final:
            raise SystemExit(f"{page_name}: reference found no box above confidence; fixture is useless")

    if not args.quiet:
        print(f"\nwrote fixtures to {fixtures_dir}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

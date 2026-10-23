"""Local ONNX sessions. No model downloads, package auto-discovery or remote code."""
import hashlib
import json
import math
from dataclasses import dataclass
from pathlib import Path
from threading import Lock
from typing import cast

import numpy as np
import onnxruntime as ort
from numpy.typing import NDArray
from PIL import Image
from rapidocr_onnxruntime import RapidOCR

from .safety import text, vector
from .tokenizer import Tokenizer

EMBED_MODEL = "siglip-base-patch16-224@4649052661e53c7000355844105f8a1792088239"
FACE_MODEL = "yunet-2023mar+sface-2021dec@f12e12798e8314f7c074a6656816c048dcc95b7a"
OCR_MODEL = "PP-OCRv4@rapidocr-1.4.4"


@dataclass(frozen=True)
class Face:
    """Normalized box, unit 128-d SFace vector, and bounded detector quality."""
    box: tuple[float, float, float, float]
    embedding: NDArray[np.float32]
    quality: float


class Models:
    """Text inference never waits for the lock serializing background model calls."""

    def __init__(self, root: Path, cpu: bool = False) -> None:
        for item in json.loads(Path(__file__).parents[1].joinpath("models.lock").read_text()):
            with (root / item["name"]).open("rb") as source:
                if hashlib.file_digest(source, "sha256").hexdigest() != item["sha256"]:
                    raise ValueError("model digest mismatch")
        providers = ["CPUExecutionProvider"] if cpu else ["CUDAExecutionProvider"]
        if not cpu and "CUDAExecutionProvider" not in ort.get_available_providers():
            raise ValueError("GPU unavailable")
        options = ort.SessionOptions()
        options.intra_op_num_threads = 2
        self.vision = ort.InferenceSession(str(root / "siglip-vision.onnx"), options, providers=providers)
        self.language = ort.InferenceSession(str(root / "siglip-text.onnx"), options, providers=providers)
        self.detector = ort.InferenceSession(str(root / "face_detection_yunet_2023mar.onnx"), options, providers=providers)
        self.recognizer = ort.InferenceSession(str(root / "face_recognition_sface_2021dec.onnx"), options, providers=providers)
        if not cpu:
            for session in (self.vision, self.language, self.detector, self.recognizer):
                if "CUDAExecutionProvider" not in session.get_providers():
                    raise ValueError("GPU session unavailable")
                session.disable_fallback()
        self.tokenizer = Tokenizer(root / "tokenizer.json")
        self.ocr = RapidOCR(det_model_path=str(root / "ocr-det.onnx"), rec_model_path=str(root / "ocr-rec.onnx"),
                            use_cls=False, det_use_cuda=not cpu, rec_use_cuda=not cpu)
        if not cpu:
            for session in (self.ocr.text_det.infer.session, self.ocr.text_rec.session.session):
                if "CUDAExecutionProvider" not in session.get_providers():
                    raise ValueError("GPU session unavailable")
                session.disable_fallback()
        self.lock = Lock()
        self.text_lock = Lock()
        self.labels = Path(__file__).parents[1].joinpath("labels.txt").read_text().splitlines()
        self.label_vectors = np.stack([self.embed_text(f"a photo of {label}") for label in self.labels])

    def embed_text(self, value: str) -> NDArray[np.float32]:
        ids = np.asarray([self.tokenizer.encode(value)], dtype=np.int64)
        with self.text_lock:
            outputs = self.language.run(None, {"input_ids": ids})
        return vector(np.asarray(outputs[1], dtype=np.float32), 768)

    def embed_image(self, image: Image.Image) -> NDArray[np.float32]:
        pixels = np.asarray(image.resize((224, 224), Image.Resampling.BICUBIC), dtype=np.float32)
        pixels = np.transpose(pixels / 127.5 - 1, (2, 0, 1))[None]
        with self.lock:
            outputs = self.vision.run(None, {"pixel_values": pixels})
        return vector(np.asarray(outputs[1], dtype=np.float32), 768)

    def tags(self, embedding: NDArray[np.float32]) -> list[tuple[str, float]]:
        similarities = np.einsum("ij,j->i", self.label_vectors, embedding)
        # Learned SigLIP calibration, from google/siglip-base-patch16-224
        # model.safetensors SHA-256 2c63cb7d1f2e95ba501893cbb8faeb4ea9a3af295498d35097126228659c2af8.
        # A .25 threshold is a classification probability, not a raw cosine.
        logits = similarities * math.exp(4.7649970054626465) - 12.9324369430542
        scores = 1 / (1 + np.exp(np.clip(-logits, -80, 80)))
        return [(text(self.labels[int(i)], 80), min(1, max(0, float(scores[i])))) for i in np.argsort(-scores)[:12] if scores[i] >= 0.25]

    def read_text(self, image: Image.Image) -> str:
        with self.lock:
            result, _ = self.ocr(np.asarray(image)[:, :, ::-1], use_cls=False)
        if result is None:
            return ""
        rows = cast(list[tuple[object, str, float]], result)
        return text(" ".join(row[1] for row in rows))

    def faces(self, image: Image.Image) -> list[Face]:
        width, height = image.size
        # YuNet's ONNX graph fixes its input to 640x640. It consumes BGR 0..255.
        scale = min(640 / width, 640 / height)
        resized = image.resize((round(width * scale), round(height * scale)), Image.Resampling.BILINEAR)
        canvas = np.zeros((640, 640, 3), dtype=np.float32)
        canvas[:resized.height, :resized.width] = np.asarray(resized, dtype=np.float32)[:, :, ::-1]
        pixels = np.transpose(canvas, (2, 0, 1))[None]
        with self.lock:
            output = dict(zip([o.name for o in self.detector.get_outputs()],
                              self.detector.run(None, {self.detector.get_inputs()[0].name: pixels}), strict=True))
        boxes: list[tuple[NDArray[np.float32], NDArray[np.float32], float]] = []
        for stride in (8, 16, 32):
            cls = np.asarray(output[f"cls_{stride}"], dtype=np.float32).reshape(-1)
            obj = np.asarray(output[f"obj_{stride}"], dtype=np.float32).reshape(-1)
            scores = np.sqrt(np.clip(cls, 0, 1) * np.clip(obj, 0, 1))
            distances = np.asarray(output[f"bbox_{stride}"], dtype=np.float32).reshape(-1, 4)
            landmarks = np.asarray(output[f"kps_{stride}"], dtype=np.float32).reshape(-1, 5, 2)
            grid = np.mgrid[:640 // stride, :640 // stride][::-1].transpose(1, 2, 0).reshape(-1, 2)
            for i in np.flatnonzero(scores >= 0.8):
                center = (grid[i] + distances[i, :2]) * stride
                size = np.exp(np.clip(distances[i, 2:], -16, 16)) * stride
                box = np.concatenate((center - size / 2, center + size / 2)) / scale
                points = (landmarks[i] + grid[i]) * stride / scale
                if np.isfinite(box).all() and np.isfinite(points).all():
                    boxes.append((np.asarray(box, dtype=np.float32), np.asarray(points, dtype=np.float32), float(scores[i])))
        boxes.sort(key=lambda item: -item[2])
        kept: list[Face] = []
        selected: list[NDArray[np.float32]] = []
        for box, points, score in boxes:
            if any(iou(box, other) > 0.4 for other in selected):
                continue
            selected.append(box)
            normalized = np.clip(box / np.asarray([width, height, width, height]), 0, 1)
            x1, y1, x2, y2 = map(float, normalized)
            if x2 - x1 < 0.01 or y2 - y1 < 0.01:
                continue
            aligned = align(image, points)
            # SFace embeds RGB 0..255; its ONNX graph includes the normalization.
            pixels = np.transpose(np.asarray(aligned, dtype=np.float32), (2, 0, 1))[None]
            with self.lock:
                outputs = self.recognizer.run(None, {self.recognizer.get_inputs()[0].name: pixels})
            kept.append(Face((x1, y1, x2, y2), vector(np.asarray(outputs[0], dtype=np.float32), 128), min(1, max(0, score))))
            if len(kept) >= 128:
                break
        return kept


def iou(a: NDArray[np.float32], b: NDArray[np.float32]) -> float:
    intersection = float(np.prod(np.maximum(0, np.minimum(a[2:], b[2:]) - np.maximum(a[:2], b[:2]))))
    union = float(np.prod(a[2:] - a[:2]) + np.prod(b[2:] - b[:2])) - intersection
    return intersection / max(union, 1e-8)


def align(image: Image.Image, points: NDArray[np.float32]) -> Image.Image:
    """Fit a similarity transform to SFace's five canonical facial landmarks."""
    target = np.asarray([[38.2946, 51.6963], [73.5318, 51.5014], [56.0252, 71.7366],
                         [41.5493, 92.3655], [70.7299, 92.2041]], dtype=np.float64)
    # Solve target -> source for Pillow's inverse affine sampling map.
    rows = []
    values = []
    for (x, y), (u, v) in zip(target, points, strict=True):
        rows.extend([[x, -y, 1, 0], [y, x, 0, 1]])
        values.extend([u, v])
    a, b, tx, ty = np.linalg.lstsq(np.asarray(rows), np.asarray(values), rcond=None)[0]
    return image.transform((112, 112), Image.Transform.AFFINE, (a, -b, tx, b, a, ty), Image.Resampling.BICUBIC)

"""Shape/determinism assertions, never semantic model-output snapshots."""
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from threading import Event

import numpy as np
import pytest
from PIL import Image

from yhotos_ml.models import Models, align
from yhotos_ml.safety import derivative, text, vector
from yhotos_ml.worker import components


def test_query_embedding_completes_while_ocr_holds_lock(models: Models, monkeypatch: pytest.MonkeyPatch) -> None:
    ocr_started, release_ocr = Event(), Event()

    def blocked_ocr(*args: object, **kwargs: object) -> tuple[None, None]:
        ocr_started.set()
        assert release_ocr.wait(timeout=10), "OCR was not released"
        return None, None

    monkeypatch.setattr(models, "ocr", blocked_ocr)
    with ThreadPoolExecutor(max_workers=2) as executor:
        ocr = executor.submit(models.read_text, Image.new("RGB", (64, 48)))
        try:
            assert ocr_started.wait(timeout=5), "OCR did not acquire its inference lock"
            query = executor.submit(models.embed_text, "sunset by the ocean").result(timeout=5)
            assert query.shape == (768,)
            assert np.isclose(np.linalg.norm(query), 1)
            assert not ocr.done()
        finally:
            release_ocr.set()
        assert ocr.result(timeout=5) == ""


@pytest.mark.parametrize("fixture", ["chart-srgb.png", "landscape.jpg", "photo.jpg"])
def test_fixture_outputs(models: Models, fixture: str) -> None:
    with Image.open(Path(__file__).parents[2] / "fixtures/images" / fixture) as image:
        image = image.convert("RGB")
        first = models.embed_image(image)
        second = models.embed_image(image)
        assert first.shape == (768,)
        np.testing.assert_allclose(first, second, atol=1e-6)
        assert np.isclose(np.linalg.norm(first), 1)
        query = models.embed_text("sunset by the ocean")
        assert query.shape == (768,)
        np.testing.assert_allclose(query, models.embed_text("sunset by the ocean"), atol=1e-6)
        tags = models.tags(first)
        assert len(tags) <= 12
        assert all(0.25 <= confidence <= 1 for _, confidence in tags)
        assert tags == models.tags(second)
        recognized = models.read_text(image)
        assert len(recognized.encode()) <= 4096
        assert recognized == models.read_text(image)
        faces = models.faces(image)
        repeated = models.faces(image)
        assert len(faces) == len(repeated)
        for face, other in zip(faces, repeated, strict=True):
            np.testing.assert_allclose(face.embedding, other.embedding, atol=1e-6)
            np.testing.assert_allclose(face.box, other.box, atol=1e-6)
            assert face.embedding.shape == (128,)
            assert all(0 <= value <= 1 for value in face.box)
            assert 0 <= face.quality <= 1


def test_untrusted_model_output_and_vectors() -> None:
    evil = "<script>alert(1)</script>\0\u202e" + "é" * 5000
    result = text(evil)
    assert result.startswith("<script>")  # Kept as text, escaped at HTTP boundaries.
    assert "\0" not in result and "\u202e" not in result
    assert len(result.encode()) <= 4096
    for bad in [np.zeros(768), np.ones(767), np.full(768, np.nan), np.full(768, np.inf)]:
        with pytest.raises(ValueError):
            vector(bad.astype(np.float32), 768)


def test_safe_derivative_only(tmp_path: Path) -> None:
    with pytest.raises(ValueError):
        derivative(tmp_path, "../originals/private")
    digest = "a" * 64
    directory = tmp_path / "derivatives" / "aa" / "aa"
    directory.mkdir(parents=True)
    file = directory / f"{digest}.large.jpeg"
    Image.new("RGB", (32, 24)).save(file)
    assert derivative(tmp_path, digest).size == (32, 24)
    file.unlink()
    original = tmp_path / "original"
    Image.new("RGB", (32, 24)).save(original, format="JPEG")
    file.symlink_to(original)
    with pytest.raises(ValueError):
        derivative(tmp_path, digest)


def test_clustering_noise_and_similarity() -> None:
    values = np.eye(128, dtype=np.float32)[:4]
    values[1] = values[0]
    assert components(values) == [[0, 1]]


def test_alignment_uses_landmarks() -> None:
    image = Image.new("RGB", (112, 112), "white")
    points = np.asarray([[38.2946, 51.6963], [73.5318, 51.5014], [56.0252, 71.7366],
                         [41.5493, 92.3655], [70.7299, 92.2041]], dtype=np.float32)
    assert align(image, points).size == (112, 112)


@pytest.mark.parametrize("fallback", ["core", "ocr"])
def test_production_rejects_silent_cpu_fallback(models: Models, monkeypatch: pytest.MonkeyPatch, fallback: str) -> None:
    import os
    import onnxruntime as ort

    class FakeSession:
        def __init__(self, *args: object, **kwargs: object) -> None:
            pass

        def get_providers(self) -> list[str]:
            return ["CPUExecutionProvider"] if fallback == "core" else ["CUDAExecutionProvider"]

        def disable_fallback(self) -> None:
            pass

    root = Path(os.environ.get("YHOTOS_ML_MODELS", Path(__file__).parents[1] / ".models"))
    monkeypatch.setattr(ort, "get_available_providers", lambda: ["CUDAExecutionProvider", "CPUExecutionProvider"])
    monkeypatch.setattr(ort, "InferenceSession", FakeSession)
    monkeypatch.setattr("yhotos_ml.models.RapidOCR", lambda **kwargs: models.ocr)
    with pytest.raises(ValueError, match="GPU session unavailable"):
        Models(root)

def test_sface_output_shape_and_determinism(models: Models) -> None:
    pixels = np.zeros((1, 3, 112, 112), dtype=np.float32)
    name = models.recognizer.get_inputs()[0].name
    first = np.asarray(models.recognizer.run(None, {name: pixels})[0])
    second = np.asarray(models.recognizer.run(None, {name: pixels})[0])
    assert first.shape == (1, 128) and np.isfinite(first).all()
    np.testing.assert_allclose(first, second, atol=1e-6)


def test_sface_clustering_uses_published_cosine_threshold() -> None:
    for similarity, expected in [(0.362, []), (0.364, [[0, 1]])]:
        values = np.zeros((2, 128), dtype=np.float32)
        values[0, 0] = 1
        values[1, :2] = [similarity, np.sqrt(1 - similarity**2)]
        assert components(values) == expected


def test_yunet_decoding_alignment_and_sface_embedding(models: Models, monkeypatch: pytest.MonkeyPatch) -> None:
    # A synthetic detector head makes this test exercise a face even when the
    # three small local photo fixtures contain no detectable people.
    output: list[np.ndarray] = []
    for tensor in models.detector.get_outputs():
        count = (640 // int(tensor.name.split("_")[1])) ** 2
        channels = 10 if tensor.name.startswith("kps") else 4 if tensor.name.startswith("bbox") else 1
        value = np.zeros((1, count, channels), dtype=np.float32)
        if tensor.name.endswith("_8"):
            index = 20 * 80 + 20
            if tensor.name.startswith(("cls", "obj")):
                value[0, index] = 0.95
            elif tensor.name.startswith("bbox"):
                value[0, index] = [0, 0, np.log(8), np.log(8)]
            else:
                value[0, index] = [-2, -1, 2, -1, 0, 1, -1.5, 3, 1.5, 3]
        output.append(value)

    def detector_run(names: object, inputs: dict[str, np.ndarray]) -> list[np.ndarray]:
        pixels = next(iter(inputs.values()))
        np.testing.assert_array_equal(pixels[0, :, 100, 100], [0, 0, 255])
        return output

    monkeypatch.setattr(models.detector, "run", detector_run)
    faces = models.faces(Image.new("RGB", (640, 640), "red"))
    assert len(faces) == 1
    assert faces[0].embedding.shape == (128,)
    assert np.isclose(np.linalg.norm(faces[0].embedding), 1)
    np.testing.assert_allclose(faces[0].box, [0.2, 0.2, 0.3, 0.3], atol=1e-6)

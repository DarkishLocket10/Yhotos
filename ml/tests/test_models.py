"""Shape/determinism assertions, never semantic model-output snapshots."""
from pathlib import Path

import numpy as np
import pytest
from PIL import Image

from yhotos_ml.models import Models, align
from yhotos_ml.safety import derivative, text, vector
from yhotos_ml.worker import components


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
            assert face.embedding.shape == (512,)
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
    values = np.eye(512, dtype=np.float32)[:4]
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

def test_arcface_output_shape_and_determinism(models: Models) -> None:
    pixels = np.zeros((1, 3, 112, 112), dtype=np.float32)
    name = models.recognizer.get_inputs()[0].name
    first = np.asarray(models.recognizer.run(None, {name: pixels})[0])
    second = np.asarray(models.recognizer.run(None, {name: pixels})[0])
    assert first.shape == (1, 512) and np.isfinite(first).all()
    np.testing.assert_allclose(first, second, atol=1e-6)

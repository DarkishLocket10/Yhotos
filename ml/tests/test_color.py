"""Local color charts must reach every model as sRGB, before model preprocessing."""
from pathlib import Path

import numpy as np
import pytest
from PIL import Image

from yhotos_ml.models import Models
from yhotos_ml.safety import derivative, srgb

FIXTURES = Path(__file__).parents[2] / "fixtures/images"


@pytest.mark.parametrize("fixture", ["chart-p3.png", "chart-adobergb.jpg", "chart-srgb.png"])
def test_profiled_pixels_match_srgb_reference(fixture: str) -> None:
    with Image.open(FIXTURES / fixture) as image, Image.open(FIXTURES / "chart-srgb.png") as reference:
        converted = srgb(image)
        assert converted.mode == "RGB"
        np.testing.assert_allclose(np.asarray(converted), np.asarray(reference), atol=2, rtol=0)
        # Neither another model call nor a resize may reinterpret converted pixels.
        assert "icc_profile" not in converted.info
        np.testing.assert_array_equal(np.asarray(srgb(converted)), np.asarray(converted))
        resized = converted.resize((224, 224))
        np.testing.assert_array_equal(np.asarray(srgb(resized)), np.asarray(resized))


def test_large_p3_jpeg_is_converted_before_leaving_loader(tmp_path: Path) -> None:
    digest = "a" * 64
    directory = tmp_path / "derivatives/aa/aa"
    directory.mkdir(parents=True)
    with Image.open(FIXTURES / "chart-p3.png") as image:
        image.save(directory / f"{digest}.large.jpeg", quality=100, subsampling=0,
                   icc_profile=image.info["icc_profile"])
    with Image.open(FIXTURES / "chart-srgb.png") as reference:
        np.testing.assert_allclose(np.asarray(derivative(tmp_path, digest)), np.asarray(reference), atol=2, rtol=0)


def test_untagged_jpeg_pixels_survive_closed_source(tmp_path: Path) -> None:
    digest = "b" * 64
    directory = tmp_path / "derivatives/bb/bb"
    directory.mkdir(parents=True)
    Image.new("RGB", (32, 24), (80, 130, 200)).save(directory / f"{digest}.large.jpeg", quality=100)
    pixels = np.asarray(derivative(tmp_path, digest))
    np.testing.assert_allclose(pixels, np.full((24, 32, 3), (80, 130, 200)), atol=1, rtol=0)


@pytest.mark.parametrize("profile", [b"", b"not an ICC profile", b"x" * (1024 * 1024 + 1), "invalid type"])
def test_invalid_profile_is_rejected(profile: object) -> None:
    image = Image.new("RGB", (32, 24))
    image.info["icc_profile"] = profile
    with pytest.raises(ValueError, match="invalid image color profile"):
        srgb(image)


@pytest.mark.parametrize("mode", ["RGB", "L", "CMYK"])
def test_untagged_images_are_rgb(mode: str) -> None:
    image = Image.new(mode, (32, 24))
    np.testing.assert_array_equal(np.asarray(srgb(image)), np.asarray(image.convert("RGB")))


@pytest.mark.parametrize("path", ["embedding", "tags", "ocr", "faces"])
def test_every_model_preprocesses_srgb_pixels(models: Models, monkeypatch: pytest.MonkeyPatch, path: str) -> None:
    captured: list[np.ndarray] = []
    if path in ("embedding", "tags"):
        def vision_run(names: object, inputs: dict[str, np.ndarray]) -> list[np.ndarray]:
            captured.append(inputs["pixel_values"].copy())
            return [np.empty(0), np.ones((1, 768), dtype=np.float32)]

        monkeypatch.setattr(models.vision, "run", vision_run)
    elif path == "ocr":
        def ocr_run(pixels: np.ndarray, **kwargs: object) -> tuple[None, None]:
            captured.append(pixels.copy())
            return None, None

        monkeypatch.setattr(models, "ocr", ocr_run)
    else:
        # Exercise SFace alignment as well as the detector, without depending on
        # actual faces being present in a synthetic color chart.
        def detector_run(names: object, inputs: dict[str, np.ndarray]) -> list[np.ndarray]:
            captured.append(next(iter(inputs.values())).copy())
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
            return output

        def recognizer_run(names: object, inputs: dict[str, np.ndarray]) -> list[np.ndarray]:
            captured.append(next(iter(inputs.values())).copy())
            return [np.ones((1, 128), dtype=np.float32)]

        monkeypatch.setattr(models.detector, "run", detector_run)
        monkeypatch.setattr(models.recognizer, "run", recognizer_run)

    with Image.open(FIXTURES / "chart-p3.png") as p3, Image.open(FIXTURES / "chart-srgb.png") as reference:
        # The regression is sensitive to the source profile (27 channel levels
        # separate the unconverted Display P3 chart from its sRGB reference).
        assert np.abs(np.asarray(p3).astype(int) - np.asarray(reference).astype(int)).max() > 20
        for image in (p3, reference):
            if path == "embedding":
                models.embed_image(image)
            elif path == "tags":
                models.tags(models.embed_image(image))
            elif path == "ocr":
                models.read_text(image)
            else:
                assert len(models.faces(image)) == 1
    count = 2 if path == "faces" else 1
    assert len(captured) == 2 * count
    for actual, expected in zip(captured[:count], captured[count:], strict=True):
        # SigLIP tensors are normalized to [-1,1]; other models use 0..255.
        tolerance = 2 / 127.5 if path in ("embedding", "tags") else 2
        np.testing.assert_allclose(actual, expected, atol=tolerance, rtol=0)

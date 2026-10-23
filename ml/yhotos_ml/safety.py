"""Bounds and canonical forms for files and untrusted model output."""
import math
import os
import re
import stat
import unicodedata
from io import BytesIO
from pathlib import Path
from typing import cast

import numpy as np
from numpy.typing import NDArray
from PIL import Image, ImageCms

Image.MAX_IMAGE_PIXELS = 2048 * 2048
SRGB = ImageCms.createProfile("sRGB")


def text(value: str, cap: int = 4096) -> str:
    """Return at most cap UTF-8 bytes, without control or format characters."""
    clean = "".join(c for c in value if not unicodedata.category(c).startswith("C"))
    return clean.encode("utf-8")[:cap].decode("utf-8", errors="ignore")


def vector(value: NDArray[np.float32], dimensions: int) -> NDArray[np.float32]:
    """Reject nonfinite, zero, or wrongly shaped output; return unit float32."""
    output = np.asarray(value, dtype=np.float32).reshape(-1)
    if output.shape != (dimensions,) or not np.isfinite(output).all():
        raise ValueError("invalid model vector")
    norm = float(np.linalg.norm(output))
    if not math.isfinite(norm) or norm < 1e-8:
        raise ValueError("invalid model vector")
    return np.asarray(output / norm, dtype=np.float32)


def srgb(image: Image.Image) -> Image.Image:
    """Return sRGB pixels; reject invalid ICC data and assume sRGB only when untagged."""
    profile = image.info.get("icc_profile")
    if profile is None:
        image.load()
        return image if image.mode == "RGB" else image.convert("RGB")
    if not isinstance(profile, bytes) or not 0 < len(profile) <= 1024 * 1024:
        raise ValueError("invalid image color profile")
    try:
        source = ImageCms.ImageCmsProfile(BytesIO(profile))
        output = cast(Image.Image, ImageCms.profileToProfile(image, source, SRGB, outputMode="RGB"))
    except (ImageCms.PyCMSError, OSError, ValueError) as error:
        raise ValueError("invalid image color profile") from error
    # Pixels now have a single canonical meaning. Drop metadata so later model
    # entrypoints do not apply the source transform again (including after resize).
    output.info.clear()
    return output


def derivative(root: Path, digest: str) -> Image.Image:
    """Return sRGB pixels from a canonical non-symlink SDR large JPEG, never an original."""
    if re.fullmatch(r"[0-9a-f]{64}", digest) is None:
        raise ValueError("invalid derivative key")
    root = root.resolve(strict=True)
    components = ["derivatives", digest[:2], digest[2:4]]
    directory = os.open(root, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        for part in components:
            child = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=directory)
            os.close(directory)
            directory = child
        descriptor = os.open(f"{digest}.large.jpeg", os.O_RDONLY | os.O_NOFOLLOW, dir_fd=directory)
    except OSError as error:
        raise ValueError("invalid derivative path") from error
    finally:
        os.close(directory)
    with os.fdopen(descriptor, "rb") as source:
        info = os.fstat(source.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_size > 32 * 1024 * 1024:
            raise ValueError("invalid derivative file")
        with Image.open(source) as image:
            if image.format != "JPEG" or max(image.size) > 2048:
                raise ValueError("invalid derivative image")
            return srgb(image)

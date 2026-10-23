"""Build-time downloader. Runtime code never imports network functionality."""
import hashlib
import importlib.resources
import shutil
import json
import sys
import urllib.request
from pathlib import Path


def download(destination: Path) -> None:
    destination.mkdir(parents=True, exist_ok=True)
    for item in json.loads(Path(__file__).with_name("models.lock").read_text()):
        target = destination / item["name"]
        if target.is_file() and hashlib.file_digest(target.open("rb"), "sha256").hexdigest() == item["sha256"]:
            continue
        temp = target.with_suffix(".partial")
        if item["url"].startswith("package:"):
            package, resource = item["url"].removeprefix("package:").split("/", 1)
            shutil.copyfile(str(importlib.resources.files(package).joinpath(resource)), temp)
        else:
            with urllib.request.urlopen(item["url"], timeout=60) as response, temp.open("wb") as output:
                while block := response.read(1024 * 1024):
                    output.write(block)
        with temp.open("rb") as source:
            if hashlib.file_digest(source, "sha256").hexdigest() != item["sha256"]:
                temp.unlink()
                raise ValueError("model digest mismatch")
        temp.replace(target)


if __name__ == "__main__":
    download(Path(sys.argv[1]))

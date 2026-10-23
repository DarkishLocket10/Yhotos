# Offline ML worker

Python 3.12; CUDA 12.8 / cuDNN 9.8 retain Pascal support. Production refuses a
missing CUDA provider. Models are fetched only by the image build, pinned by
revision and SHA-256 in models.lock, and verified again at startup. SigLIP
ViT-B/16 produces normalized 768-dimensional vectors. Tags use its learned
logit scale/bias and sigmoid probabilities at threshold 0.25; YuNet detects faces
and SFace produces 128-dimensional private face vectors. Both ONNX graphs run
directly in ONNX Runtime; alignment uses NumPy/Pillow. See MODELS.md for licences
and the SFace clustering threshold. RapidOCR supplies pinned PP-OCRv4 assets.
OpenCV headless is required only by RapidOCR's OCR preprocessing. Its metadata
requires the desktop package, deliberately replaced by the headless closure.
Install both CPU and GPU closures with --no-deps as the Dockerfile does.

The GPU requirements deliberately omit the CPU onnxruntime distribution required
by RapidOCR metadata: both distributions provide the same Python module. Install
the fully pinned/hash-checked closure with --no-deps as the Dockerfile does.
requirements-cpu.txt provides the alternative CPU closure used by CI.

For local CPU tests:

    python3.12 -m venv .venv
    .venv/bin/pip install --no-deps --require-hashes -r requirements-cpu.txt -r requirements-dev.txt
    PATH="$PWD/.venv/bin:$PATH" make models
    DATABASE_URL=<disposable-admin-database-url> PATH="$PWD/.venv/bin:$PATH" make check AUDIT_REQUIREMENTS=requirements-cpu.txt

Tests create and remove their own randomly named database, using the repository
migrations. Three photographs come from fixtures/images. No photo fixture comes
from the network. Full pipeline/GPU timing must run on tower with its fixture
library; CPU timings are correctness evidence only.

Connect Postgres and the HTTP containers to the internal yhotos-internal Docker
network before starting deploy/compose.yml. Do not attach ML to an egress-enabled
network. Supply the worker login with TLS verification and its CA certificate.
The data directory is read-only; the worker resolves only large SDR JPEG keys.
Embedded ICC profiles are converted to sRGB before resizing, normalization, face
alignment or OCR channel ordering. Embedding and tag jobs share this conversion;
direct image-model calls enforce it too. Untagged images are assumed sRGB, and
invalid or oversized (over 1 MiB) profiles fail the job. The codec resolves NCLX
source metadata to an ICC profile before encoding these JPEG derivatives, so
NCLX originals follow the same conversion without decoding originals in ML.
Set YHOTOS_DATA_GID to the storage writer's group (default 10001) so the non-root
ML user can traverse its directories and read 0640 derivatives.
HTTP uses YHOTOS_ML_ADDRESS=ml:8090, 100 ms total timeout, a thousand-entry ten-minute
cache, and text-only fallback. The default ml-client Cargo feature enables the
online text-embedding call; builds without it retain text and filter searches.
Search results always use private, no-store.

Queue wire kinds are ml.embed, ml.faces, ml.ocr, ml.tags, ml.cluster. CPU workers
no longer claim these. Upload follow-ups retain their lower priority. Inference
runs outside transactions; publication rechecks the job attempt, worker identity,
photo state and derivative digest in one transaction. A five-minute watchdog
terminates a hung native model; the six-minute stale lease recovers its claim.
Nightly clustering is deduplicated by UTC day and admin rebuilds use the typed
queue's envelope deduplication. Hidden and deleted identities are excluded.

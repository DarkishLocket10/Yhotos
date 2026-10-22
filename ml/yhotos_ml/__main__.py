"""Private embedding HTTP service and background queue loop."""
import logging
import os
import threading
from pathlib import Path

import uvicorn

from .models import Models
from .app import create_app
from .worker import STOP, run

logging.basicConfig(level=logging.INFO)
models = Models(Path(os.environ.get("YHOTOS_ML_MODELS", "/app/models")),
                cpu=os.environ.get("YHOTOS_ML_CPU") == "1")
app = create_app(models)


def main() -> None:
    worker = threading.Thread(target=run, args=(os.environ["YHOTOS_DATABASE_URL_WORKER"], Path("/data"), models), daemon=True)
    worker.start()
    try:
        uvicorn.run(app, host="0.0.0.0", port=8090, access_log=False, limit_concurrency=20, timeout_keep_alive=1)
    finally:
        STOP.set()
        worker.join(timeout=30)


if __name__ == "__main__":
    main()

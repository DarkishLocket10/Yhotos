"""Small internal endpoint with a streaming request cap, independent of model loading."""
import asyncio
import json
from typing import Protocol

import numpy as np
from fastapi import FastAPI, HTTPException, Request
from numpy.typing import NDArray

from .safety import text


class Embedder(Protocol):
    def embed_text(self, value: str) -> NDArray[np.float32]: ...


def create_app(models: Embedder) -> FastAPI:
    app = FastAPI(docs_url=None, redoc_url=None, openapi_url=None)


    @app.post("/embed-text")
    async def embed(request: Request) -> dict[str, list[float]]:
        # Read and bound before JSON parsing, including chunked bodies.
        body = bytearray()
        async for chunk in request.stream():
            body.extend(chunk)
            if len(body) > 2048:
                raise HTTPException(413, "Request too large")
        try:
            data = json.loads(body)
        except (ValueError, UnicodeError) as error:
            raise HTTPException(400, "Invalid request") from error
        if not isinstance(data, dict) or set(data) != {"text"} or not isinstance(data["text"], str):
            raise HTTPException(400, "Invalid request")
        value = data["text"]
        if not value or len(value) > 256 or value != text(value):
            raise HTTPException(400, "Invalid request")
        # Inference never blocks the ASGI executor.
        result = await asyncio.to_thread(models.embed_text, value)
        return {"vector": result.tolist()}

    return app

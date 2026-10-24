"""Exercise raw ASGI requests without adding an HTTP-client dependency."""
import asyncio
import json

import numpy as np

from yhotos_ml.app import create_app


class FixtureEmbedder:
    def embed_text(self, value: str) -> np.ndarray:
        return np.asarray([1] + [0] * 767, dtype=np.float32)


def request(body: bytes) -> tuple[int, bytes]:
    app = create_app(FixtureEmbedder())
    messages = []

    async def receive() -> dict:
        return {"type": "http.request", "body": body, "more_body": False}

    async def send(message: dict) -> None:
        messages.append(message)

    asyncio.run(app({"type": "http", "http_version": "1.1", "method": "POST", "scheme": "http",
                     "path": "/embed-text", "raw_path": b"/embed-text", "query_string": b"",
                     "headers": [(b"content-type", b"application/json")],
                     "server": ("ml", 8090), "client": ("127.0.0.1", 1)}, receive, send))
    return messages[0]["status"], b"".join(m.get("body", b"") for m in messages[1:])


def test_embed_endpoint_shape_and_body_limits() -> None:
    status, body = request(b'{"text":"sunset"}')
    assert status == 200
    assert len(json.loads(body)["vector"]) == 768
    for body in [b'{}', b'{"text":1}', b'{"text":"\\u0000"}', b"invalid"]:
        assert request(body)[0] == 400
    assert request(b"x" * 2049)[0] == 413
    assert request(json.dumps({"text": "a" * 257}).encode())[0] == 400

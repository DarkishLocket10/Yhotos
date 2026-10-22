FROM nvidia/cuda:12.8.1-cudnn-runtime-ubuntu24.04@sha256:ac55d124da4882b497f732d8dfd9a702d5447a5f29d08d56da6f64f0a1eb34bc
RUN apt-get update && apt-get install -y --no-install-recommends python3.12 python3.12-venv ca-certificates libglib2.0-0t64 && rm -rf /var/lib/apt/lists/*
WORKDIR /app
RUN python3.12 -m venv /opt/venv
COPY ml/requirements.txt .
# CPU onnxruntime is deliberately omitted from the GPU dependency closure.
RUN /opt/venv/bin/pip install --no-cache-dir --no-deps --require-hashes -r requirements.txt
COPY ml/models.lock ml/download.py ./
RUN /opt/venv/bin/python download.py /app/models
COPY ml/labels.txt ml/MODELS.md ./
COPY ml/licenses ./licenses
COPY ml/yhotos_ml ./yhotos_ml
RUN useradd --uid 10001 --no-create-home --shell /usr/sbin/nologin ml && chmod -R a=rX /app
ENV PATH="/opt/venv/bin:$PATH" PYTHONDONTWRITEBYTECODE=1 PYTHONUNBUFFERED=1
USER 10001:10001
EXPOSE 8090
ENTRYPOINT ["python", "-m", "yhotos_ml"]

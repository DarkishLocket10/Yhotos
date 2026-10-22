"""SKIP LOCKED claims, LISTEN wakeups, fenced transactions and private clustering."""
import json
import logging
import os
import signal
import threading
import time
import uuid
from pathlib import Path
from typing import TypeAlias

import numpy as np
import psycopg
from numpy.typing import NDArray
from psycopg.rows import dict_row

from .models import EMBED_MODEL, FACE_MODEL, OCR_MODEL, Models
from .safety import derivative, vector

Connection: TypeAlias = psycopg.Connection[dict[str, object]]
KINDS = ["ml.embed", "ml.faces", "ml.ocr", "ml.tags", "ml.cluster"]
CLAIM = """
UPDATE jobs SET locked_by=%s,locked_at=now(),attempts=attempts+1
WHERE id=(SELECT id FROM jobs WHERE finished_at IS NULL AND locked_at IS NULL
 AND run_after<=now() AND attempts<max_attempts AND kind=ANY(%s)
 ORDER BY priority DESC,run_after,id LIMIT 1 FOR UPDATE SKIP LOCKED)
RETURNING id,kind,payload,attempts,max_attempts
"""
STOP = threading.Event()


def identifier() -> uuid.UUID:
    """Time-ordered UUIDv7, using independent OS randomness for its random fields."""
    value = ((time.time_ns() // 1_000_000) << 80) | (uuid.uuid4().int & ((1 << 76) - 1))
    value = (value & ~(15 << 76)) | (7 << 76)
    value = (value & ~(3 << 62)) | (2 << 62)
    return uuid.UUID(int=value)


def connect(url: str) -> Connection:
    from psycopg.conninfo import conninfo_to_dict
    from ipaddress import ip_address
    options = conninfo_to_dict(url)
    host = options.get("host", "")
    if not isinstance(host, str):
        raise ValueError("invalid database host")
    local = os.environ.get("YHOTOS_ML_LOCAL_TEST") == "1"
    if local and not (host.startswith("/") or ip_address(host).is_loopback):
        raise ValueError("plaintext database must be local")
    conn = psycopg.connect(url, autocommit=True, row_factory=dict_row,
                           sslmode="disable" if local else "verify-full")
    conn.execute("SET ROLE yhotos_worker")
    conn.execute("SET statement_timeout='60000'")
    conn.execute("SET idle_in_transaction_session_timeout='30000'")
    return conn


def claim(conn: Connection, worker: str) -> dict[str, object] | None:
    return conn.execute(CLAIM, (worker, KINDS)).fetchone()


def serialize(vector: NDArray[np.float32]) -> str:
    return "[" + ",".join(str(float(v)) for v in vector) + "]"


def process(conn: Connection, models: Models, root: Path, worker: str, job: dict[str, object]) -> None:
    """Inference precedes a single transaction that validates the claim and source again."""
    kind = str(job["kind"])
    photo: uuid.UUID | None = None
    digest: str | None = None
    payload = job["payload"]
    if kind != "ml.cluster":
        if not isinstance(payload, dict) or set(payload) != {"photo_id"}:
            raise ValueError("invalid job envelope")
        photo = uuid.UUID(str(payload["photo_id"]))
        row = conn.execute("""
            SELECT d.hash FROM derivatives d JOIN photos p ON p.id=d.photo_id
            WHERE d.photo_id=%s AND d.variant='large' AND d.format='jpeg' AND NOT d.hdr
              AND p.deleted_at IS NULL AND p.status='ready'
            """, (photo,)).fetchone()
        if row is None:
            raise ValueError("large derivative unavailable")
        digest = str(row["hash"])
        image = derivative(root, digest)
        if kind in ("ml.embed", "ml.tags"):
            embedding = models.embed_image(image)
        elif kind == "ml.faces":
            faces = models.faces(image)
        elif kind == "ml.ocr":
            ocr = models.read_text(image)
        else:
            raise ValueError("invalid job kind")
    if STOP.is_set():
        return
    with conn.transaction():
        owned = conn.execute("""
            SELECT id FROM jobs WHERE id=%s AND locked_by=%s AND attempts=%s
              AND finished_at IS NULL AND locked_at IS NOT NULL FOR UPDATE
            """, (job["id"], worker, job["attempts"])).fetchone()
        if owned is None:
            return
        if photo is not None:
            conn.execute("SELECT id FROM photos WHERE id=%s FOR UPDATE", (photo,))
            current = conn.execute("""
                SELECT d.hash FROM derivatives d JOIN photos p ON p.id=d.photo_id
                WHERE d.photo_id=%s AND d.variant='large' AND d.format='jpeg' AND NOT d.hdr
                AND p.status='ready' AND p.deleted_at IS NULL
                """, (photo,)).fetchone()
            if current is None or current["hash"] != digest:
                raise ValueError("source derivative changed")
        if kind == "ml.embed":
            conn.execute("""
                INSERT INTO embeddings(photo_id,model,vector) VALUES(%s,%s,%s::vector)
                ON CONFLICT(photo_id) DO UPDATE SET model=excluded.model,vector=excluded.vector,updated_at=now()
                """, (photo, EMBED_MODEL, serialize(embedding)))
        elif kind == "ml.faces":
            conn.execute("DELETE FROM faces WHERE photo_id=%s AND model=%s", (photo, FACE_MODEL))
            for face in faces:
                conn.execute("""
                    INSERT INTO faces(id,photo_id,model,bbox,embedding,quality)
                    VALUES(%s,%s,%s,%s,%s::vector,%s)
                    """, (identifier(), photo, FACE_MODEL, list(face.box), serialize(face.embedding), face.quality))
        elif kind == "ml.ocr":
            conn.execute("""
                INSERT INTO ocr_text(id,photo_id,model,text) VALUES(%s,%s,%s,%s)
                ON CONFLICT(photo_id) DO UPDATE SET model=excluded.model,text=excluded.text,updated_at=now()
                """, (identifier(), photo, OCR_MODEL, ocr))
        elif kind == "ml.tags":
            conn.execute("DELETE FROM photo_tags WHERE photo_id=%s AND source='auto' AND model=%s", (photo, EMBED_MODEL))
            for label, confidence in models.tags(embedding):
                slug = label.lower().replace(" ", "-")
                row = conn.execute("""
                    INSERT INTO tags(id,slug,name,source) VALUES(%s,%s,%s,'auto')
                    ON CONFLICT(slug) DO UPDATE SET slug=excluded.slug RETURNING id
                    """, (identifier(), slug, label)).fetchone()
                if row is None:
                    raise ValueError("tag insertion failed")
                # A manually curated association is never overwritten.
                conn.execute("""
                    INSERT INTO photo_tags(id,photo_id,tag_id,source,model,confidence)
                    VALUES(%s,%s,%s,'auto',%s,%s) ON CONFLICT(photo_id,tag_id) DO UPDATE
                    SET model=excluded.model,confidence=excluded.confidence WHERE photo_tags.source='auto'
                    """, (identifier(), photo, row["id"], EMBED_MODEL, confidence))
        else:
            cluster(conn)
        conn.execute("""
            UPDATE jobs SET finished_at=now(),locked_at=NULL,locked_by=NULL,error=NULL
            WHERE id=%s AND locked_by=%s AND attempts=%s
            """, (job["id"], worker, job["attempts"]))


def components(vectors: NDArray[np.float32]) -> list[list[int]]:
    """DBSCAN with min_samples=2 (including self), cosine eps=.6, bounded block memory."""
    parents = list(range(len(vectors)))

    def find(i: int) -> int:
        while parents[i] != i:
            parents[i] = parents[parents[i]]
            i = parents[i]
        return i

    for start in range(0, len(vectors), 128):
        scores = vectors[start:start + 128] @ vectors.T
        for offset, row in enumerate(scores):
            i = start + offset
            for j in np.flatnonzero(row >= 0.4):
                if int(j) > i:
                    parents[find(int(j))] = find(i)
    groups: dict[int, list[int]] = {}
    for i in range(len(vectors)):
        groups.setdefault(find(i), []).append(i)
    return [g for g in groups.values() if len(g) >= 2]


def cluster(conn: Connection) -> None:
    # One clustering transaction globally; hidden persons and deleted photos stay out.
    conn.execute("SELECT pg_advisory_xact_lock(140014)")
    rows = conn.execute("""
        SELECT f.id,f.embedding::text AS embedding,f.person_id,p.name
        FROM faces f JOIN photos photo ON photo.id=f.photo_id
        LEFT JOIN persons p ON p.id=f.person_id
        WHERE f.model=%s AND photo.deleted_at IS NULL AND coalesce(p.hidden,false)=false
        ORDER BY f.id FOR UPDATE OF f
        """, (FACE_MODEL,)).fetchall()
    if not rows:
        return
    vectors = np.stack([vector(np.asarray(json.loads(str(r["embedding"])), dtype=np.float32), 512) for r in rows])
    for group in components(vectors):
        named = {rows[i]["person_id"] for i in group if rows[i]["name"]}
        existing = {rows[i]["person_id"] for i in group if rows[i]["person_id"] is not None}
        pending = [rows[i]["id"] for i in group if rows[i]["person_id"] is None]
        if not pending or len(named) > 1:
            continue
        person = next(iter(named or existing), None)
        if person is None:
            person = identifier()
            conn.execute("INSERT INTO persons(id) VALUES(%s)", (person,))
        conn.execute("UPDATE faces SET person_id=%s WHERE id=ANY(%s) AND person_id IS NULL", (person, pending))


def fail(conn: Connection, worker: str, job: dict[str, object]) -> None:
    attempts = int(str(job["attempts"]))
    conn.execute("""
        UPDATE jobs SET error='ML processing failed',run_after=now()+make_interval(secs=>%s),
          finished_at=CASE WHEN attempts>=max_attempts THEN now() ELSE NULL END,locked_at=NULL,locked_by=NULL
        WHERE id=%s AND locked_by=%s AND attempts=%s AND finished_at IS NULL
        """, (min(3600, 30 * 2 ** min(attempts, 7)), job["id"], worker, attempts))


def maintain(conn: Connection) -> None:
    conn.execute("""
        UPDATE jobs SET locked_at=NULL,locked_by=NULL,
          finished_at=CASE WHEN attempts>=max_attempts THEN now() ELSE NULL END,
          error='ML worker lock expired' WHERE kind=ANY(%s) AND finished_at IS NULL
          AND locked_at<now()-interval '6 minutes'
        """, (KINDS,))
    # UTC day key prevents duplicate nightly rebuilds, including after a restart.
    with conn.transaction():
        conn.execute("SELECT pg_advisory_xact_lock(hashtextextended(%s,0))", ("ml.cluster:null",))
        conn.execute("""
            INSERT INTO jobs(id,kind,payload,priority)
            SELECT %s,'ml.cluster','null'::jsonb,-20 WHERE NOT EXISTS (
                SELECT FROM jobs WHERE kind='ml.cluster' AND (finished_at IS NULL OR created_at>=(now() AT TIME ZONE 'UTC')::date AT TIME ZONE 'UTC'))
            """, (identifier(),))
        conn.execute("SELECT pg_notify('jobs','')")


def run(url: str, root: Path, models: Models) -> None:
    worker = "ml-" + str(identifier())
    while not STOP.is_set():
        try:
            with connect(url) as conn, connect(url) as listener:
                listener.execute("LISTEN jobs")
                maintained = 0.0
                while not STOP.is_set():
                    if time.monotonic() - maintained > 60:
                        maintain(conn)
                        maintained = time.monotonic()
                    job = claim(conn, worker)
                    if job is None:
                        # Three-second polling also covers lost notifications/reconnections.
                        for _ in listener.notifies(timeout=3, stop_after=1):
                            pass
                        continue
                    # A hung native model loses its process; the six-minute stale-lock
                    # lease then recovers it. Never let a timed-out thread publish results.
                    watchdog = threading.Timer(300, lambda: os._exit(1))
                    watchdog.daemon = True
                    watchdog.start()
                    started = time.perf_counter()
                    try:
                        process(conn, models, root, worker, job)
                        logging.info("ML job completed kind=%s elapsed_ms=%.2f", job["kind"], (time.perf_counter()-started)*1000)
                    except Exception:
                        fail(conn, worker, job)
                        logging.warning("ML job failed kind=%s", job["kind"])
                    finally:
                        watchdog.cancel()
                conn.execute("""
                    UPDATE jobs SET locked_at=NULL,locked_by=NULL,attempts=greatest(0,attempts-1)
                    WHERE locked_by=%s AND finished_at IS NULL
                    """, (worker,))
        except psycopg.Error:
            logging.warning("ML database unavailable")
            STOP.wait(3)


def install_signals() -> None:
    for signum in (signal.SIGTERM, signal.SIGINT):
        signal.signal(signum, lambda _s, _f: STOP.set())

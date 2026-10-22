"""Real database protocol tests include stale ownership and replay idempotency."""
import uuid
from pathlib import Path

import numpy as np
import psycopg
from PIL import Image

from yhotos_ml.models import EMBED_MODEL, FACE_MODEL, Models
from yhotos_ml.worker import Connection, claim, cluster, identifier, process, serialize


def seed(conn: Connection, root: Path) -> uuid.UUID:
    album, photo, digest = identifier(), identifier(), uuid.uuid4().hex * 2
    conn.execute("INSERT INTO albums(id,slug,title) VALUES(%s,%s,'fixture')", (album, "fixture-" + uuid.uuid4().hex))
    conn.execute("""
        INSERT INTO photos(id,album_id,short_id,title,original_hash,original_name,original_bytes,mime,kind,status)
        VALUES(%s,%s,%s,'fixture',%s,'fixture.jpg',1,'image/jpeg','image','ready')
        """, (photo, album, uuid.uuid4().hex.translate(str.maketrans("0189", "abcz"))[:12], digest))
    conn.execute("""
        INSERT INTO derivatives(id,photo_id,variant,format,hash,width,height,bytes)
        VALUES(%s,%s,'large','jpeg',%s,64,48,1)
        """, (identifier(), photo, digest))
    directory = root / "derivatives" / digest[:2] / digest[2:4]
    directory.mkdir(parents=True)
    Image.new("RGB", (64, 48), "red").save(directory / f"{digest}.large.jpeg")
    return photo


def test_claims_retries_and_listen(connection: Connection, database: str) -> None:
    from psycopg.rows import dict_row
    connection.execute("INSERT INTO jobs(id,kind,payload,priority) VALUES(%s,'ingest','{}',100)", (identifier(),))
    job = identifier()
    connection.execute("INSERT INTO jobs(id,kind,payload,priority) VALUES(%s,'ml.embed','{}',-10)", (job,))
    with psycopg.connect(database, autocommit=True, row_factory=dict_row) as other:
        other.execute("SET ROLE yhotos_worker")
        row = claim(connection, "worker-a")
        assert row is not None and row["id"] == job
        assert claim(other, "worker-b") is None
        other.execute("LISTEN jobs")
        connection.execute("SELECT pg_notify('jobs','')")
        assert len(list(other.notifies(timeout=1, stop_after=1))) == 1
        connection.execute("UPDATE jobs SET locked_by=NULL,locked_at=NULL WHERE id=%s", (job,))
        replacement = claim(other, "worker-a")
        assert replacement is not None and replacement["attempts"] == 2
        connection.execute("""
            UPDATE jobs SET finished_at=now() WHERE id=%s AND locked_by='worker-a' AND attempts=1
            """, (job,))
        assert connection.execute("SELECT finished_at FROM jobs WHERE id=%s", (job,)).fetchone()["finished_at"] is None


def test_each_job_replay_is_idempotent(connection: Connection, tmp_path: Path, models: Models) -> None:
    from psycopg.types.json import Jsonb
    photo = seed(connection, tmp_path)
    for kind in ["ml.embed", "ml.faces", "ml.ocr", "ml.tags"]:
        for _ in range(2):
            job_id = identifier()
            connection.execute("INSERT INTO jobs(id,kind,payload) VALUES(%s,%s,%s)", (job_id, kind, Jsonb({"photo_id": str(photo)})))
            job = claim(connection, "fixture-worker")
            assert job is not None
            process(connection, models, tmp_path, "fixture-worker", job)
            result = connection.execute("SELECT finished_at,error FROM jobs WHERE id=%s", (job_id,)).fetchone()
            assert result is not None and result["finished_at"] is not None and result["error"] is None
    count = connection.execute("SELECT count(*) AS count FROM embeddings WHERE photo_id=%s AND model=%s", (photo, EMBED_MODEL)).fetchone()
    assert count is not None and count["count"] == 1
    count = connection.execute("SELECT count(*) AS count FROM ocr_text WHERE photo_id=%s", (photo,)).fetchone()
    assert count is not None and count["count"] == 1


def test_named_persons_and_hidden_clusters_never_merge(connection: Connection, tmp_path: Path) -> None:
    photo = seed(connection, tmp_path)
    people = [identifier() for _ in range(3)]
    for i, person in enumerate(people):
        connection.execute("INSERT INTO persons(id,name,hidden) VALUES(%s,%s,%s)", (person, f"name-{i}", i == 2))
    embedding = serialize(np.asarray([1] + [0] * 511, dtype=np.float32))
    for person in [*people, None, None]:
        connection.execute("""
            INSERT INTO faces(id,photo_id,bbox,embedding,quality,model,person_id)
            VALUES(%s,%s,ARRAY[0.1,0.1,0.5,0.5],%s::vector,0.9,%s,%s)
            """, (identifier(), photo, embedding, FACE_MODEL, person))
    with connection.transaction():
        cluster(connection)
    assert connection.execute("SELECT count(*) AS count FROM persons").fetchone()["count"] == 3
    assert connection.execute("SELECT count(*) AS count FROM faces WHERE person_id IS NULL").fetchone()["count"] == 2


def test_nightly_rebuild_deduplicates_and_stale_locks_recover(connection: Connection) -> None:
    from yhotos_ml.worker import maintain
    maintain(connection)
    first = claim(connection, "nightly-worker")
    assert first is not None and first["kind"] == "ml.cluster"
    connection.execute("UPDATE jobs SET locked_at=now()-interval '7 minutes' WHERE id=%s", (first["id"],))
    maintain(connection)
    second = claim(connection, "replacement-worker")
    assert second is not None and second["id"] == first["id"] and second["attempts"] == 2
    assert connection.execute("SELECT count(*) AS count FROM jobs WHERE kind='ml.cluster'").fetchone()["count"] == 1

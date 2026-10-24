"""Tiny local fixtures and a disposable real database, never downloaded photos."""
import os
import uuid
from collections.abc import Iterator
from pathlib import Path

import psycopg
import pytest
from psycopg import sql
from psycopg.conninfo import make_conninfo
from psycopg.rows import dict_row

from yhotos_ml.models import Models


@pytest.fixture(scope="session")
def models() -> Models:
    root = Path(os.environ.get("YHOTOS_ML_MODELS", Path(__file__).parents[1] / ".models"))
    if not root.is_dir():
        pytest.fail("Download pinned models with make models before running correctness tests")
    return Models(root, cpu=True)


@pytest.fixture(scope="session")
def database() -> Iterator[str]:
    url = os.environ.get("DATABASE_URL")
    if not url:
        pytest.fail("DATABASE_URL required for the real Postgres queue tests")
    name = "ml_test_" + uuid.uuid4().hex
    with psycopg.connect(url, autocommit=True) as admin:
        admin.execute(sql.SQL("CREATE DATABASE {}").format(sql.Identifier(name)))
    test_url = make_conninfo(url, dbname=name)
    try:
        with psycopg.connect(test_url, autocommit=True) as conn:
            for migration in sorted(Path(__file__).parents[2].joinpath("crates/yhotos-db/migrations").glob("*.sql")):
                conn.execute(migration.read_text())
        yield test_url
    finally:
        with psycopg.connect(url, autocommit=True) as admin:
            admin.execute(sql.SQL("DROP DATABASE {} WITH (FORCE)").format(sql.Identifier(name)))


@pytest.fixture(params=["UTC", "America/Toronto"])
def connection(database: str, request: pytest.FixtureRequest) -> Iterator[psycopg.Connection[dict[str, object]]]:
    with psycopg.connect(database, autocommit=True, row_factory=dict_row) as conn:
        conn.execute("SELECT set_config('TimeZone',%s,false)", (request.param,))
        yield conn
        conn.execute("TRUNCATE albums,photos,persons,tags,jobs CASCADE")

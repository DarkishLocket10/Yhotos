#!/usr/bin/env python3
"""Validate a local GeoNames cities500 file and atomically bulk-copy it via psql."""
import csv
import math
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time
from urllib.parse import urlsplit, unquote, parse_qsl


def main():
    started = time.monotonic()
    if len(sys.argv) != 2 or not os.environ.get("DATABASE_URL"):
        raise ValueError("local file and DATABASE_URL required")
    source = Path(sys.argv[1]).resolve(strict=True)
    if not source.is_file():
        raise ValueError("input must be a regular local file")
    # Spool validated data before opening a transaction; bounded process memory.
    with tempfile.TemporaryFile(mode="w+", encoding="utf-8", newline="") as sql:
        sql.write("BEGIN;\nCREATE TEMP TABLE geo_import (LIKE gazetteer EXCLUDING GENERATED);\n")
        sql.write("COPY geo_import (geonameid,name,admin1,country,lat,lon,population) FROM STDIN WITH (FORMAT csv);\n")
        writer = csv.writer(sql, lineterminator="\n")
        count = 0
        with source.open(encoding="utf-8", newline="") as data:
            for line in iter(lambda: data.readline(65537), ""):
                if len(line) > 65536:
                    raise ValueError("oversized input row")
                fields = line.rstrip("\r\n").split("\t")
                if len(fields) != 19:
                    raise ValueError("expected 19 GeoNames columns")
                geonameid, name, admin1, country = int(fields[0]), fields[1], fields[10], fields[8]
                lat, lon, population = float(fields[4]), float(fields[5]), int(fields[14])
                if not (0 < geonameid <= 2**63-1 and 0 <= population <= 2**63-1
                        and math.isfinite(lat) and -90 <= lat <= 90
                        and math.isfinite(lon) and -180 <= lon <= 180
                        and re.fullmatch(r"[A-Z]{2}", country)
                        and 1 <= len(name) <= 200 and len(admin1) <= 80
                        and not any(ord(c) < 32 for c in name + admin1)):
                    raise ValueError("invalid GeoNames row")
                writer.writerow([geonameid, name, admin1, country, lat, lon, population])
                count += 1
        if not count:
            raise ValueError("input contains no cities")
        sql.write("\\.\nINSERT INTO gazetteer (geonameid,name,admin1,country,lat,lon,population)\n"
                  "SELECT geonameid,name,admin1,country,lat,lon,population FROM geo_import\n"
                  "ON CONFLICT (geonameid) DO UPDATE SET name=excluded.name,admin1=excluded.admin1,\n"
                  "country=excluded.country,lat=excluded.lat,lon=excluded.lon,population=excluded.population;\n"
                  "ANALYZE gazetteer;\nCOMMIT;\n")
        sql.seek(0)
        # URL stays in the child's environment; neither argv nor errors print it.
        url = urlsplit(os.environ["DATABASE_URL"])
        if url.scheme not in ("postgres", "postgresql") or not url.hostname:
            raise ValueError("PostgreSQL URL required")
        env = dict(os.environ, PGHOST=unquote(url.hostname), PGPORT=str(url.port or 5432),
                   PGDATABASE=unquote(url.path.lstrip("/")))
        if url.username is not None:
            env["PGUSER"] = unquote(url.username)
        if url.password is not None:
            env["PGPASSWORD"] = unquote(url.password)
        params = {"host":"PGHOST", "port":"PGPORT", "user":"PGUSER", "password":"PGPASSWORD",
                  "dbname":"PGDATABASE", "sslmode":"PGSSLMODE", "sslrootcert":"PGSSLROOTCERT",
                  "sslcert":"PGSSLCERT", "sslkey":"PGSSLKEY", "options":"PGOPTIONS",
                  "connect_timeout":"PGCONNECT_TIMEOUT", "application_name":"PGAPPNAME"}
        for key, value in parse_qsl(url.query, keep_blank_values=True):
            if key not in params:
                raise ValueError("unsupported database URL option")
            env[params[key]] = value
        result = subprocess.run(["psql", "-X", "--no-password", "-v", "ON_ERROR_STOP=1", "-q"],
                                stdin=sql, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env=env, check=False)
        if result.returncode:
            raise ValueError("database import failed; transaction rolled back")
    print(f"Imported {count} cities in {time.monotonic() - started:.2f}s")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, UnicodeError):
        print("GeoNames import failed: check the local file, database and migration.", file=sys.stderr)
        sys.exit(1)

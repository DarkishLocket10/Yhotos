CREATE TABLE uploads (
 id uuid PRIMARY KEY,
 user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
 album_id uuid NOT NULL REFERENCES albums(id) ON DELETE CASCADE,
 filename text NOT NULL CHECK (octet_length(filename) BETWEEN 1 AND 255),
 declared_bytes bigint NOT NULL CHECK (declared_bytes > 0),
 received_bytes bigint NOT NULL DEFAULT 0 CHECK (received_bytes >= 0 AND received_bytes <= declared_bytes),
 mime_hint text NOT NULL CHECK (octet_length(mime_hint) <= 255),
 tmp_path text NOT NULL UNIQUE CHECK (tmp_path ~ '^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$'),
 expires_at timestamptz NOT NULL DEFAULT now()+interval '24 hours',
 created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX uploads_expiry ON uploads(expires_at);
CREATE INDEX uploads_owner ON uploads(user_id);
GRANT ALL ON uploads TO yhotos_admin, yhotos_worker;

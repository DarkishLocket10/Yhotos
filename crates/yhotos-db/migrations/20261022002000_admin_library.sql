-- Old originals are durable GC roots for thirty days after replacement.
CREATE TABLE original_trash (
 id uuid PRIMARY KEY, photo_id uuid, original_hash text NOT NULL CHECK (original_hash ~ '^[0-9a-f]{64}$'),
 original_name text NOT NULL, original_bytes bigint NOT NULL CHECK(original_bytes >= 0),
 created_at timestamptz NOT NULL DEFAULT now(), expires_at timestamptz NOT NULL DEFAULT now()+interval '30 days'
);
CREATE INDEX original_trash_hash ON original_trash(original_hash);
-- A durable set of candidates lets GC avoid traversing the originals tree.
CREATE TABLE original_gc (original_hash text PRIMARY KEY CHECK(original_hash ~ '^[0-9a-f]{64}$'));
ALTER TABLE users ADD COLUMN deleted_at timestamptz;
ALTER TABLE uploads ADD COLUMN replace_photo_id uuid REFERENCES photos(id);
CREATE TABLE cache_purges (
 id uuid PRIMARY KEY, tags text[] NOT NULL, created_at timestamptz NOT NULL DEFAULT now()
);
GRANT ALL ON original_trash, original_gc, cache_purges TO yhotos_admin, yhotos_worker;

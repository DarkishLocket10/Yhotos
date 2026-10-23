-- Invalidate incompatible legacy vectors while preserving face ids, boxes and
-- person assignments. The worker reconciles these boxes when SFace first runs.
DROP INDEX faces_embedding;
ALTER TABLE faces ALTER COLUMN embedding DROP NOT NULL;
ALTER TABLE faces ALTER COLUMN embedding TYPE vector(128) USING NULL::vector(128);
CREATE INDEX faces_embedding ON faces USING hnsw (embedding vector_cosine_ops);
CREATE TABLE face_runs (
 photo_id uuid NOT NULL REFERENCES photos ON DELETE CASCADE,
 model text NOT NULL, source_hash text NOT NULL CHECK (source_hash ~ '^[0-9a-f]{64}$'),
 PRIMARY KEY(photo_id,model)
);
CREATE TABLE face_crops (
 face_id uuid PRIMARY KEY REFERENCES faces ON DELETE CASCADE,
 source_hash text NOT NULL CHECK (source_hash ~ '^[0-9a-f]{64}$'),
 bbox real[] NOT NULL CHECK (array_length(bbox,1)=4),
 hash text NOT NULL CHECK (hash ~ '^[0-9a-f]{64}$'),
 bytes bigint NOT NULL CHECK (bytes BETWEEN 1 AND 1048576)
);
GRANT SELECT,INSERT,UPDATE,DELETE ON face_runs,face_crops TO yhotos_worker,yhotos_admin;
REVOKE ALL ON face_runs,face_crops FROM yhotos_public;

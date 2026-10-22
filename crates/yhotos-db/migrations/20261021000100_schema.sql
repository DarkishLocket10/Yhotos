CREATE EXTENSION IF NOT EXISTS pgcrypto;
CREATE EXTENSION IF NOT EXISTS citext;
CREATE EXTENSION IF NOT EXISTS vector;

CREATE TYPE visibility AS ENUM ('inherit', 'public', 'restricted');
CREATE TYPE principal_kind AS ENUM ('user', 'group');
CREATE TYPE target_kind AS ENUM ('album', 'photo');
CREATE TYPE media_kind AS ENUM ('image', 'raw', 'video');
CREATE TYPE tag_source AS ENUM ('manual', 'auto');
CREATE TYPE flag_resolution AS ENUM ('replaced', 'reedited', 'removed', 'visibility', 'dismissed');

CREATE FUNCTION maintain_updated_at() RETURNS trigger LANGUAGE plpgsql
SET search_path = pg_catalog AS $$
BEGIN NEW.updated_at = clock_timestamp(); RETURN NEW; END
$$;

CREATE TABLE users (
 id uuid PRIMARY KEY, created_at timestamptz NOT NULL DEFAULT now(), updated_at timestamptz NOT NULL DEFAULT now(),
 username citext NOT NULL UNIQUE CHECK (length(username) BETWEEN 1 AND 80), password_hash text NOT NULL,
 must_change_password boolean NOT NULL DEFAULT true, is_admin boolean NOT NULL DEFAULT false,
 disabled_at timestamptz, last_login_at timestamptz
);
CREATE TABLE user_mfa (
 id uuid PRIMARY KEY, created_at timestamptz NOT NULL DEFAULT now(), updated_at timestamptz NOT NULL DEFAULT now(),
 user_id uuid NOT NULL REFERENCES users ON DELETE CASCADE, kind text NOT NULL CHECK (kind IN ('totp','passkey','email')),
 secret_enc bytea, credential_id bytea UNIQUE, public_key bytea, sign_count bigint NOT NULL DEFAULT 0,
 label text NOT NULL, enabled_at timestamptz
);
CREATE TABLE sessions (
 id uuid PRIMARY KEY, created_at timestamptz NOT NULL DEFAULT now(), updated_at timestamptz NOT NULL DEFAULT now(),
 user_id uuid NOT NULL REFERENCES users ON DELETE CASCADE, token_hash bytea NOT NULL UNIQUE,
 last_seen_at timestamptz NOT NULL DEFAULT now(), reauth_at timestamptz, expires_at timestamptz NOT NULL,
 ip_hash bytea NOT NULL, ua_class text NOT NULL
);
CREATE INDEX sessions_user ON sessions(user_id);
CREATE TABLE groups (
 id uuid PRIMARY KEY, created_at timestamptz NOT NULL DEFAULT now(), updated_at timestamptz NOT NULL DEFAULT now(), name text NOT NULL UNIQUE
);
CREATE TABLE user_groups (
 id uuid PRIMARY KEY, created_at timestamptz NOT NULL DEFAULT now(),
 user_id uuid NOT NULL REFERENCES users ON DELETE CASCADE, group_id uuid NOT NULL REFERENCES groups ON DELETE CASCADE,
 UNIQUE (user_id, group_id)
);
CREATE INDEX user_groups_group ON user_groups(group_id);
CREATE TABLE albums (
 id uuid PRIMARY KEY, created_at timestamptz NOT NULL DEFAULT now(), updated_at timestamptz NOT NULL DEFAULT now(),
 parent_id uuid REFERENCES albums, slug citext NOT NULL CHECK (slug::text ~ '^[a-z0-9]+(-[a-z0-9]+)*$' AND length(slug) <= 80),
 title text NOT NULL, description text NOT NULL DEFAULT '', sort_order integer NOT NULL DEFAULT 0,
 cover_photo_id uuid, visibility visibility NOT NULL DEFAULT 'public' CHECK (visibility <> 'inherit'),
 manual_order boolean NOT NULL DEFAULT false, deleted_at timestamptz,
 CHECK (parent_id IS DISTINCT FROM id), CHECK (id <> '00000000-0000-0000-0000-000000000000'::uuid)
);
CREATE UNIQUE INDEX albums_parent_slug ON albums(coalesce(parent_id, '00000000-0000-0000-0000-000000000000'::uuid), slug);
CREATE INDEX albums_parent_sort ON albums(parent_id, sort_order);
CREATE TABLE album_grants (
 id uuid PRIMARY KEY, created_at timestamptz NOT NULL DEFAULT now(), album_id uuid NOT NULL REFERENCES albums ON DELETE CASCADE,
 principal_kind principal_kind NOT NULL, principal_id uuid NOT NULL, UNIQUE (album_id, principal_kind, principal_id)
);
CREATE TABLE places (
 id uuid PRIMARY KEY, created_at timestamptz NOT NULL DEFAULT now(), updated_at timestamptz NOT NULL DEFAULT now(),
 name text NOT NULL, admin1 text, country text NOT NULL, lat double precision NOT NULL CHECK (lat BETWEEN -90 AND 90),
 lon double precision NOT NULL CHECK (lon BETWEEN -180 AND 180)
);
CREATE TABLE photos (
 id uuid PRIMARY KEY, created_at timestamptz NOT NULL DEFAULT now(), updated_at timestamptz NOT NULL DEFAULT now(),
 short_id text NOT NULL UNIQUE CHECK (short_id ~ '^[a-z2-7]{12}$'), album_id uuid NOT NULL REFERENCES albums,
 title text NOT NULL, description text NOT NULL DEFAULT '', taken_at timestamptz, posted_at timestamptz NOT NULL DEFAULT now(),
 sort_order integer NOT NULL DEFAULT 0, visibility visibility NOT NULL DEFAULT 'inherit',
 original_hash text NOT NULL CHECK (original_hash ~ '^[0-9a-f]{64}$'), original_name text NOT NULL,
 original_bytes bigint NOT NULL CHECK (original_bytes >= 0), mime text NOT NULL, kind media_kind NOT NULL,
 width integer CHECK (width > 0), height integer CHECK (height > 0), orientation integer CHECK (orientation BETWEEN 1 AND 8),
 is_hdr boolean NOT NULL DEFAULT false, has_gain_map boolean NOT NULL DEFAULT false,
 icc_name text, camera_make text, camera_model text, lens text, focal_mm double precision, aperture double precision,
 shutter text, iso integer CHECK (iso > 0), gps_lat double precision CHECK (gps_lat BETWEEN -90 AND 90),
 gps_lon double precision CHECK (gps_lon BETWEEN -180 AND 180), place_id uuid REFERENCES places,
 dominant_colors integer[] NOT NULL DEFAULT '{}', placeholder bytea, deleted_at timestamptz
);
ALTER TABLE albums ADD CONSTRAINT albums_cover_fkey FOREIGN KEY (cover_photo_id) REFERENCES photos ON DELETE SET NULL;
CREATE INDEX photos_album_sort ON photos(album_id, sort_order, id);
CREATE INDEX photos_taken ON photos(taken_at);
CREATE INDEX photos_hash ON photos(original_hash);
CREATE TABLE photo_grants (
 id uuid PRIMARY KEY, created_at timestamptz NOT NULL DEFAULT now(), photo_id uuid NOT NULL REFERENCES photos ON DELETE CASCADE,
 principal_kind principal_kind NOT NULL, principal_id uuid NOT NULL, UNIQUE (photo_id, principal_kind, principal_id)
);
CREATE TABLE derivatives (
 id uuid PRIMARY KEY, created_at timestamptz NOT NULL DEFAULT now(), photo_id uuid NOT NULL REFERENCES photos ON DELETE CASCADE,
 variant text NOT NULL, format text NOT NULL CHECK (format IN ('avif','jxl','jpeg')),
 hdr boolean NOT NULL DEFAULT false, hash text NOT NULL CHECK (hash ~ '^[0-9a-f]{64}$'),
 width integer NOT NULL CHECK (width > 0), height integer NOT NULL CHECK (height > 0), bytes bigint NOT NULL CHECK (bytes >= 0),
 UNIQUE (photo_id, variant, format, hdr)
);
CREATE TABLE video_renditions (
 id uuid PRIMARY KEY, created_at timestamptz NOT NULL DEFAULT now(), photo_id uuid NOT NULL REFERENCES photos ON DELETE CASCADE,
 codec text NOT NULL CHECK (codec IN ('h264','av1')), hash text NOT NULL CHECK (hash ~ '^[0-9a-f]{64}$'),
 width integer NOT NULL CHECK (width > 0), height integer NOT NULL CHECK (height > 0), bytes bigint NOT NULL CHECK (bytes >= 0),
 duration_ms bigint NOT NULL CHECK (duration_ms >= 0), hls boolean NOT NULL DEFAULT false, UNIQUE (photo_id, codec, hls)
);
CREATE TABLE tags (
 id uuid PRIMARY KEY, created_at timestamptz NOT NULL DEFAULT now(), updated_at timestamptz NOT NULL DEFAULT now(),
 slug citext NOT NULL UNIQUE CHECK (slug::text ~ '^[a-z0-9]+(-[a-z0-9]+)*$' AND length(slug) <= 80),
 name text NOT NULL, source tag_source NOT NULL, hidden boolean NOT NULL DEFAULT false
);
CREATE TABLE photo_tags (
 id uuid PRIMARY KEY, created_at timestamptz NOT NULL DEFAULT now(), photo_id uuid NOT NULL REFERENCES photos ON DELETE CASCADE,
 tag_id uuid NOT NULL REFERENCES tags ON DELETE CASCADE, confidence real CHECK (confidence BETWEEN 0 AND 1),
 source tag_source NOT NULL, UNIQUE (photo_id, tag_id)
);
CREATE INDEX photo_tags_tag ON photo_tags(tag_id, photo_id);
CREATE TABLE share_links (
 id uuid PRIMARY KEY, created_at timestamptz NOT NULL DEFAULT now(), updated_at timestamptz NOT NULL DEFAULT now(),
 token_hash bytea NOT NULL UNIQUE CHECK (octet_length(token_hash) = 32), target_kind target_kind NOT NULL, target_id uuid NOT NULL,
 passphrase_hash text, expires_at timestamptz, allow_view boolean NOT NULL DEFAULT true, allow_download boolean NOT NULL DEFAULT false,
 allow_like boolean NOT NULL DEFAULT false, allow_flag boolean NOT NULL DEFAULT false,
 revoked_at timestamptz, created_by uuid NOT NULL REFERENCES users
);
CREATE TABLE share_link_opens (
 id uuid PRIMARY KEY, created_at timestamptz NOT NULL DEFAULT now(), share_link_id uuid NOT NULL REFERENCES share_links ON DELETE CASCADE,
 opened_at timestamptz NOT NULL DEFAULT now(), visitor_hash bytea NOT NULL, country text, city text
);
CREATE INDEX share_link_opens_link ON share_link_opens(share_link_id, opened_at);
CREATE TABLE likes (
 id uuid PRIMARY KEY, created_at timestamptz NOT NULL DEFAULT now(), user_id uuid REFERENCES users ON DELETE CASCADE,
 share_link_id uuid REFERENCES share_links ON DELETE CASCADE, photo_id uuid NOT NULL REFERENCES photos ON DELETE CASCADE,
 CHECK ((user_id IS NULL) <> (share_link_id IS NULL)), UNIQUE (user_id, photo_id), UNIQUE (share_link_id, photo_id)
);
CREATE INDEX likes_photo ON likes(photo_id);
CREATE TABLE flags (
 id uuid PRIMARY KEY, created_at timestamptz NOT NULL DEFAULT now(), updated_at timestamptz NOT NULL DEFAULT now(),
 user_id uuid REFERENCES users ON DELETE CASCADE, share_link_id uuid REFERENCES share_links ON DELETE CASCADE,
 photo_id uuid NOT NULL REFERENCES photos ON DELETE CASCADE, resolved_at timestamptz, resolution flag_resolution,
 CHECK ((user_id IS NULL) <> (share_link_id IS NULL)), CHECK ((resolved_at IS NULL) = (resolution IS NULL))
);
CREATE INDEX flags_open ON flags(created_at, id) WHERE resolved_at IS NULL;
CREATE TABLE persons (
 id uuid PRIMARY KEY, created_at timestamptz NOT NULL DEFAULT now(), updated_at timestamptz NOT NULL DEFAULT now(),
 name text, hidden boolean NOT NULL DEFAULT false
);
CREATE TABLE faces (
 id uuid PRIMARY KEY, created_at timestamptz NOT NULL DEFAULT now(), photo_id uuid NOT NULL REFERENCES photos ON DELETE CASCADE,
 bbox real[] NOT NULL CHECK (array_length(bbox, 1) = 4), embedding vector(512) NOT NULL, person_id uuid REFERENCES persons,
 quality real NOT NULL CHECK (quality BETWEEN 0 AND 1)
);
CREATE INDEX faces_embedding ON faces USING hnsw (embedding vector_cosine_ops);
CREATE TABLE embeddings (
 photo_id uuid PRIMARY KEY REFERENCES photos ON DELETE CASCADE, created_at timestamptz NOT NULL DEFAULT now(),
 updated_at timestamptz NOT NULL DEFAULT now(), model text NOT NULL, vector vector(768) NOT NULL
);
CREATE INDEX embeddings_vector ON embeddings USING hnsw (vector vector_cosine_ops);
CREATE TABLE ocr_text (
 id uuid PRIMARY KEY, created_at timestamptz NOT NULL DEFAULT now(), updated_at timestamptz NOT NULL DEFAULT now(),
 photo_id uuid NOT NULL UNIQUE REFERENCES photos ON DELETE CASCADE, text text NOT NULL,
 tsv tsvector GENERATED ALWAYS AS (to_tsvector('simple', text)) STORED
);
CREATE INDEX ocr_text_tsv ON ocr_text USING gin (tsv);
CREATE TABLE jobs (
 id uuid PRIMARY KEY, created_at timestamptz NOT NULL DEFAULT now(), updated_at timestamptz NOT NULL DEFAULT now(),
 kind text NOT NULL CHECK (length(kind) BETWEEN 1 AND 80), payload jsonb NOT NULL, priority integer NOT NULL DEFAULT 0,
 run_after timestamptz NOT NULL DEFAULT now(), attempts integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
 max_attempts integer NOT NULL DEFAULT 5 CHECK (max_attempts > 0), locked_by text, locked_at timestamptz,
 finished_at timestamptz, error text, CHECK ((locked_at IS NULL) = (locked_by IS NULL))
);
CREATE INDEX jobs_kind_run ON jobs(kind, run_after) WHERE finished_at IS NULL;
CREATE INDEX jobs_claim ON jobs(priority DESC, run_after, id) WHERE finished_at IS NULL AND locked_at IS NULL;
CREATE TABLE events (
 id uuid PRIMARY KEY, created_at timestamptz NOT NULL DEFAULT now(), at timestamptz NOT NULL DEFAULT now(), kind text NOT NULL,
 photo_id uuid REFERENCES photos ON DELETE SET NULL, album_id uuid REFERENCES albums ON DELETE SET NULL,
 share_link_id uuid REFERENCES share_links ON DELETE SET NULL, visitor_hash bytea NOT NULL, country text, city text,
 device_class text, referrer_host text, extra jsonb NOT NULL DEFAULT '{}'
);
CREATE INDEX events_at ON events(at);
CREATE TABLE events_daily (
 id uuid PRIMARY KEY, created_at timestamptz NOT NULL DEFAULT now(), updated_at timestamptz NOT NULL DEFAULT now(),
 day date NOT NULL, kind text NOT NULL, target_kind target_kind NOT NULL, target_id uuid NOT NULL,
 count bigint NOT NULL CHECK (count >= 0), UNIQUE (day, kind, target_kind, target_id)
);
CREATE TABLE audit_log (
 id uuid PRIMARY KEY, created_at timestamptz NOT NULL DEFAULT now(), at timestamptz NOT NULL DEFAULT now(), user_id uuid REFERENCES users,
 action text NOT NULL, target_kind text, target_id uuid, detail jsonb NOT NULL DEFAULT '{}'
);
CREATE TABLE settings (key text PRIMARY KEY, value jsonb NOT NULL, updated_at timestamptz NOT NULL DEFAULT now());
CREATE TABLE legacy_redirects (
 id uuid PRIMARY KEY, created_at timestamptz NOT NULL DEFAULT now(), updated_at timestamptz NOT NULL DEFAULT now(),
 pattern text NOT NULL UNIQUE CHECK (pattern LIKE '/%' AND pattern NOT LIKE '//%'), target_kind target_kind NOT NULL, target_id uuid NOT NULL
);
CREATE TRIGGER updated_at BEFORE UPDATE ON users FOR EACH ROW EXECUTE FUNCTION maintain_updated_at();
CREATE TRIGGER updated_at BEFORE UPDATE ON user_mfa FOR EACH ROW EXECUTE FUNCTION maintain_updated_at();
CREATE TRIGGER updated_at BEFORE UPDATE ON sessions FOR EACH ROW EXECUTE FUNCTION maintain_updated_at();
CREATE TRIGGER updated_at BEFORE UPDATE ON groups FOR EACH ROW EXECUTE FUNCTION maintain_updated_at();
CREATE TRIGGER updated_at BEFORE UPDATE ON albums FOR EACH ROW EXECUTE FUNCTION maintain_updated_at();
CREATE TRIGGER updated_at BEFORE UPDATE ON places FOR EACH ROW EXECUTE FUNCTION maintain_updated_at();
CREATE TRIGGER updated_at BEFORE UPDATE ON photos FOR EACH ROW EXECUTE FUNCTION maintain_updated_at();
CREATE TRIGGER updated_at BEFORE UPDATE ON tags FOR EACH ROW EXECUTE FUNCTION maintain_updated_at();
CREATE TRIGGER updated_at BEFORE UPDATE ON share_links FOR EACH ROW EXECUTE FUNCTION maintain_updated_at();
CREATE TRIGGER updated_at BEFORE UPDATE ON flags FOR EACH ROW EXECUTE FUNCTION maintain_updated_at();
CREATE TRIGGER updated_at BEFORE UPDATE ON persons FOR EACH ROW EXECUTE FUNCTION maintain_updated_at();
CREATE TRIGGER updated_at BEFORE UPDATE ON embeddings FOR EACH ROW EXECUTE FUNCTION maintain_updated_at();
CREATE TRIGGER updated_at BEFORE UPDATE ON ocr_text FOR EACH ROW EXECUTE FUNCTION maintain_updated_at();
CREATE TRIGGER updated_at BEFORE UPDATE ON jobs FOR EACH ROW EXECUTE FUNCTION maintain_updated_at();
CREATE TRIGGER updated_at BEFORE UPDATE ON events_daily FOR EACH ROW EXECUTE FUNCTION maintain_updated_at();
CREATE TRIGGER updated_at BEFORE UPDATE ON settings FOR EACH ROW EXECUTE FUNCTION maintain_updated_at();
CREATE TRIGGER updated_at BEFORE UPDATE ON legacy_redirects FOR EACH ROW EXECUTE FUNCTION maintain_updated_at();

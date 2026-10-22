-- Preserve tag identities when Piwigo names or generated slugs change.
CREATE TABLE migration_tags (
 source_id bigint PRIMARY KEY CHECK (source_id > 0),
 tag_id uuid NOT NULL UNIQUE REFERENCES tags ON DELETE CASCADE
);
GRANT ALL ON migration_tags TO yhotos_admin;

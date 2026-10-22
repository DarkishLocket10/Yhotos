-- Retain only upload ownership long enough to recognize a competing final PATCH.
CREATE TABLE completed_uploads (
 id uuid PRIMARY KEY,
 user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
 expires_at timestamptz NOT NULL DEFAULT now()+interval '24 hours',
 created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX completed_uploads_expiry ON completed_uploads(expires_at);
GRANT ALL ON completed_uploads TO yhotos_admin, yhotos_worker;

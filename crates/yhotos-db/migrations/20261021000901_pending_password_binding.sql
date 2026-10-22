-- A password reset must invalidate unfinished MFA logins too. Existing pending
-- logins fail closed until they expire; new ones bind the verified hash version.
ALTER TABLE pending_login ADD COLUMN password_version bytea CHECK (octet_length(password_version)=32);
ALTER TABLE pending_login ADD COLUMN password_at timestamptz NOT NULL DEFAULT now();

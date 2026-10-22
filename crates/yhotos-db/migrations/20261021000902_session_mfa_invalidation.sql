-- Existing sessions have never proved a second factor for this bearer.
ALTER TABLE sessions ADD COLUMN mfa_at timestamptz;

-- Notifications contain row ids only and are delivered after transaction commit.
-- Triggers also cover revocations made by another trusted component.
CREATE FUNCTION session_cache_invalidate() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
BEGIN
 PERFORM pg_notify('session_invalidate', 'session:' || OLD.id::text);
 RETURN NULL;
END $$;
CREATE TRIGGER session_cache_deleted AFTER DELETE ON sessions
 FOR EACH ROW EXECUTE FUNCTION session_cache_invalidate();
CREATE TRIGGER session_cache_changed AFTER UPDATE OF reauth_at, mfa_at, admin, expires_at ON sessions
 FOR EACH ROW EXECUTE FUNCTION session_cache_invalidate();

CREATE FUNCTION user_session_cache_invalidate() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
BEGIN
 PERFORM pg_notify('session_invalidate', 'user:' || NEW.id::text);
 RETURN NULL;
END $$;
CREATE TRIGGER user_session_cache_changed
 AFTER UPDATE OF password_hash, must_change_password, is_admin, disabled_at ON users
 FOR EACH ROW EXECUTE FUNCTION user_session_cache_invalidate();
REVOKE ALL ON FUNCTION session_cache_invalidate(), user_session_cache_invalidate() FROM PUBLIC;

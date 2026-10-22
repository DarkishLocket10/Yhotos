-- One public share bearer unlocks a bounded set, independently of account sessions.
ALTER TABLE sessions ADD COLUMN is_share boolean NOT NULL DEFAULT false;
UPDATE sessions SET is_share=true WHERE share_link_id IS NOT NULL;
ALTER TABLE sessions DROP CONSTRAINT sessions_exactly_one_actor;
ALTER TABLE sessions DROP CONSTRAINT sessions_share_public;
ALTER TABLE sessions ADD CONSTRAINT sessions_exactly_one_actor
 CHECK ((user_id IS NULL) = is_share);
ALTER TABLE sessions ADD CONSTRAINT sessions_share_public
 CHECK (NOT is_share OR (NOT admin AND mfa_at IS NULL AND reauth_at IS NULL));

CREATE TABLE share_session_links (
 session_id uuid NOT NULL REFERENCES sessions ON DELETE CASCADE,
 share_link_id uuid NOT NULL REFERENCES share_links ON DELETE CASCADE,
 unlocked_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 PRIMARY KEY (session_id, share_link_id)
);
CREATE INDEX share_session_links_link ON share_session_links(share_link_id);
INSERT INTO share_session_links(session_id,share_link_id,unlocked_at)
 SELECT id,share_link_id,last_seen_at FROM sessions WHERE is_share;
ALTER TABLE sessions DROP COLUMN share_link_id;
GRANT SELECT,INSERT,UPDATE,DELETE ON share_session_links TO yhotos_public,yhotos_admin;

-- Serialize additions on the session row, including concurrent visits. An existing
-- membership only refreshes its recency; a new one evicts the least recent member.
CREATE FUNCTION share_session_bound() RETURNS trigger
LANGUAGE plpgsql SET search_path=pg_catalog,pg_temp AS $$
BEGIN
 PERFORM 1 FROM public.sessions WHERE id=NEW.session_id AND is_share FOR UPDATE;
 IF NOT FOUND THEN
  RAISE EXCEPTION 'invalid share session' USING ERRCODE='23514';
 END IF;
 IF NOT EXISTS (SELECT FROM public.share_session_links
                WHERE session_id=NEW.session_id AND share_link_id=NEW.share_link_id) THEN
  DELETE FROM public.share_session_links WHERE session_id=NEW.session_id AND share_link_id IN (
   SELECT share_link_id FROM public.share_session_links WHERE session_id=NEW.session_id
   ORDER BY unlocked_at DESC,share_link_id DESC OFFSET 49
  );
 END IF;
 -- Generate recency after acquiring the row lock, rather than before waiting.
 NEW.unlocked_at=clock_timestamp();
 RETURN NEW;
END $$;
CREATE TRIGGER share_session_bounded BEFORE INSERT ON share_session_links
 FOR EACH ROW EXECUTE FUNCTION share_session_bound();
REVOKE ALL ON FUNCTION share_session_bound() FROM PUBLIC;

CREATE OR REPLACE FUNCTION share_cache_invalidate() RETURNS trigger
LANGUAGE plpgsql SET search_path=pg_catalog,pg_temp AS $$
BEGIN
 PERFORM pg_notify('session_invalidate', 'link:' || OLD.id::text);
 IF TG_OP='DELETE' OR NEW.revoked_at IS NOT NULL
    OR (NEW.expires_at IS NOT NULL AND NEW.expires_at<=clock_timestamp())
    OR NOT NEW.allow_view OR NEW.passphrase_hash IS DISTINCT FROM OLD.passphrase_hash THEN
  DELETE FROM public.share_session_links WHERE share_link_id=OLD.id;
 END IF;
 RETURN NULL;
END $$;

-- Removing one grant invalidates cached session sets in the other public process.
CREATE FUNCTION share_session_removed() RETURNS trigger
LANGUAGE plpgsql SET search_path=pg_catalog,pg_temp AS $$
BEGIN
 PERFORM pg_notify('session_invalidate', 'session:' || OLD.session_id::text);
 RETURN NULL;
END $$;
CREATE TRIGGER share_session_link_removed AFTER DELETE ON share_session_links
 FOR EACH ROW EXECUTE FUNCTION share_session_removed();
REVOKE ALL ON FUNCTION share_session_removed() FROM PUBLIC;

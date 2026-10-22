-- Grant matrix (row ownership and authenticated user ids are application responsibilities):
-- admin: ALL tables, migration owner controls DDL.
-- public: SELECT all except user_mfa/audit_log; INSERT events/likes/flags/share_link_opens/sessions/audit_log;
--         DELETE sessions/likes; UPDATE sessions(last_seen_at, reauth_at); EXECUTE password/login functions.
-- worker: ALL except users/user_mfa/settings/audit_log; no EXECUTE on password/login functions.
-- Login roles are provisioned by deployment and inherit exactly one of these NOLOGIN roles.
DO $$ BEGIN
 BEGIN CREATE ROLE yhotos_public NOLOGIN; EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL; END;
 BEGIN CREATE ROLE yhotos_admin NOLOGIN; EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL; END;
 BEGIN CREATE ROLE yhotos_worker NOLOGIN; EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL; END;
END $$;
REVOKE CREATE ON SCHEMA public FROM PUBLIC;
GRANT USAGE ON SCHEMA public TO yhotos_public, yhotos_admin, yhotos_worker;
GRANT ALL PRIVILEGES ON ALL TABLES IN SCHEMA public TO yhotos_admin;
-- Do not expose the migration bookkeeping table to application roles.
GRANT SELECT ON users, sessions, groups, user_groups, albums, album_grants, places, photos,
 photo_grants, derivatives, video_renditions, tags, photo_tags, share_links, share_link_opens,
 likes, flags, persons, faces, embeddings, ocr_text, jobs, events, events_daily, settings, legacy_redirects TO yhotos_public;
GRANT INSERT ON events, likes, flags, share_link_opens, sessions, audit_log TO yhotos_public;
GRANT DELETE ON sessions, likes TO yhotos_public;
GRANT UPDATE (last_seen_at, reauth_at) ON sessions TO yhotos_public;
GRANT ALL PRIVILEGES ON sessions, groups, user_groups, albums, album_grants, places, photos,
 photo_grants, derivatives, video_renditions, tags, photo_tags, share_links, share_link_opens,
 likes, flags, persons, faces, embeddings, ocr_text, jobs, events, events_daily, legacy_redirects TO yhotos_worker;

CREATE FUNCTION user_set_own_password(user_id uuid, new_hash text) RETURNS void
LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
 UPDATE public.users SET password_hash = new_hash, must_change_password = false, updated_at = clock_timestamp()
 WHERE id = user_id;
$$;
CREATE FUNCTION user_touch_login(user_id uuid) RETURNS void
LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS $$
 UPDATE public.users SET last_login_at = clock_timestamp(), updated_at = clock_timestamp() WHERE id = user_id;
$$;
-- Ownership transfer requires schema CREATE briefly; the role retains no DDL grant.
GRANT CREATE ON SCHEMA public TO yhotos_admin;
ALTER FUNCTION user_set_own_password(uuid, text) OWNER TO yhotos_admin;
ALTER FUNCTION user_touch_login(uuid) OWNER TO yhotos_admin;
REVOKE CREATE ON SCHEMA public FROM yhotos_admin;
REVOKE ALL ON FUNCTION user_set_own_password(uuid, text), user_touch_login(uuid) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION user_set_own_password(uuid, text), user_touch_login(uuid) TO yhotos_public, yhotos_admin;

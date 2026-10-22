-- MFA backoff belongs to the account, across challenges, listeners and IPs.
ALTER TABLE users ADD COLUMN mfa_failures integer NOT NULL DEFAULT 0 CHECK (mfa_failures >= 0);
ALTER TABLE users ADD COLUMN mfa_locked_until timestamptz;
CREATE FUNCTION user_set_mfa_lockout(uid uuid, failures integer, deadline timestamptz)
RETURNS void LANGUAGE sql SECURITY DEFINER SET search_path=pg_catalog,pg_temp AS $$
 UPDATE public.users SET mfa_failures=failures,mfa_locked_until=deadline WHERE id=uid;
$$;

-- Historical credentials have no trustworthy RP binding. Leave them unbound;
-- password-only setup sessions can enroll a credential for the current listener.
ALTER TABLE user_mfa ADD COLUMN rp_id text
 CHECK (rp_id IS NULL OR (kind='passkey' AND length(rp_id) BETWEEN 1 AND 253));
DROP FUNCTION user_mfa_add(uuid,uuid,text,bytea,bytea,bytea,bigint,bigint);
CREATE FUNCTION user_mfa_add(mid uuid, uid uuid, method text, sealed bytea, cid bytea, pkey bytea, counter bigint, step bigint, rp text)
RETURNS void LANGUAGE sql SECURITY DEFINER SET search_path=pg_catalog,pg_temp AS $$
 INSERT INTO public.user_mfa(id,user_id,kind,secret_enc,credential_id,public_key,sign_count,label,enabled_at,last_used_step,rp_id)
 SELECT mid,uid,method,sealed,cid,pkey,counter,CASE WHEN method='totp' THEN 'Authenticator app' ELSE 'Passkey' END,clock_timestamp(),step,rp
 WHERE (method='totp' AND rp IS NULL) OR (method='passkey' AND rp IS NOT NULL);
$$;
GRANT CREATE ON SCHEMA public TO yhotos_admin;
ALTER FUNCTION user_set_mfa_lockout(uuid,integer,timestamptz) OWNER TO yhotos_admin;
ALTER FUNCTION user_mfa_add(uuid,uuid,text,bytea,bytea,bytea,bigint,bigint,text) OWNER TO yhotos_admin;
REVOKE CREATE ON SCHEMA public FROM yhotos_admin;
REVOKE ALL ON FUNCTION user_set_mfa_lockout(uuid,integer,timestamptz),user_mfa_add(uuid,uuid,text,bytea,bytea,bytea,bigint,bigint,text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION user_set_mfa_lockout(uuid,integer,timestamptz),user_mfa_add(uuid,uuid,text,bytea,bytea,bytea,bigint,bigint,text) TO yhotos_public,yhotos_admin;

CREATE FUNCTION mfa_session_cache_invalidate() RETURNS trigger
LANGUAGE plpgsql SET search_path=pg_catalog,pg_temp AS $$
BEGIN
 PERFORM pg_notify('session_invalidate', 'user:' || COALESCE(NEW.user_id,OLD.user_id)::text);
 RETURN NULL;
END $$;
CREATE TRIGGER mfa_session_cache_changed AFTER INSERT OR UPDATE OR DELETE ON user_mfa
 FOR EACH ROW EXECUTE FUNCTION mfa_session_cache_invalidate();
REVOKE ALL ON FUNCTION mfa_session_cache_invalidate() FROM PUBLIC;

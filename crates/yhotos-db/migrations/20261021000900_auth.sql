-- Audience binding prevents public bearer tokens from acquiring admin rights.
ALTER TABLE sessions ADD COLUMN admin boolean NOT NULL DEFAULT false;
ALTER TABLE user_mfa ADD COLUMN last_used_step bigint;
CREATE TABLE pending_login (
 id uuid PRIMARY KEY, token_hash bytea NOT NULL UNIQUE CHECK (octet_length(token_hash)=32),
 user_id uuid NOT NULL REFERENCES users ON DELETE CASCADE, admin boolean NOT NULL,
 next_path text, attempts integer NOT NULL DEFAULT 0 CHECK (attempts BETWEEN 0 AND 5),
 expires_at timestamptz NOT NULL DEFAULT (now()+interval '5 minutes')
);
CREATE INDEX pending_login_expiry ON pending_login(expires_at);
CREATE TABLE pending_webauthn (
 binding_id uuid NOT NULL, kind text NOT NULL CHECK (kind IN ('register','authenticate','totp')),
 user_id uuid NOT NULL REFERENCES users ON DELETE CASCADE, state jsonb NOT NULL,
 expires_at timestamptz NOT NULL DEFAULT (now()+interval '5 minutes'),
 PRIMARY KEY(binding_id,kind)
);
CREATE INDEX pending_webauthn_expiry ON pending_webauthn(expires_at);
CREATE TABLE auth_lockout (
 key bytea PRIMARY KEY CHECK (octet_length(key)=32), state jsonb NOT NULL,
 updated_at timestamptz NOT NULL DEFAULT now()
);
GRANT SELECT, INSERT, UPDATE, DELETE ON pending_login, pending_webauthn, auth_lockout TO yhotos_public, yhotos_admin;
-- No direct grants on user_mfa: each operation is bound to an authenticated user
-- by the application, as with user_set_own_password. Fixed search paths and an
-- unprivileged owner prevent function search-path/DDL escalation.
CREATE FUNCTION user_mfa_list(uid uuid) RETURNS SETOF public.user_mfa
LANGUAGE sql SECURITY DEFINER SET search_path=pg_catalog,pg_temp AS $$
 SELECT * FROM public.user_mfa WHERE user_id=uid AND enabled_at IS NOT NULL;
$$;
CREATE FUNCTION user_mfa_add(mid uuid, uid uuid, method text, sealed bytea, cid bytea, pkey bytea, counter bigint, step bigint)
RETURNS void LANGUAGE sql SECURITY DEFINER SET search_path=pg_catalog,pg_temp AS $$
 INSERT INTO public.user_mfa(id,user_id,kind,secret_enc,credential_id,public_key,sign_count,label,enabled_at,last_used_step)
 SELECT mid,uid,method,sealed,cid,pkey,counter,CASE WHEN method='totp' THEN 'Authenticator app' ELSE 'Passkey' END,clock_timestamp(),step
 WHERE method IN ('totp','passkey');
$$;
CREATE FUNCTION user_mfa_use(mid uuid, uid uuid, step bigint) RETURNS boolean
LANGUAGE sql SECURITY DEFINER SET search_path=pg_catalog,pg_temp AS $$
 WITH changed AS (UPDATE public.user_mfa SET last_used_step=step
 WHERE id=mid AND user_id=uid AND kind='totp' AND enabled_at IS NOT NULL
 AND (last_used_step IS NULL OR last_used_step<step) RETURNING id)
 SELECT EXISTS(SELECT FROM changed);
$$;
CREATE FUNCTION user_passkey_use(mid uuid, uid uuid, old_count bigint, new_count bigint, pkey bytea) RETURNS boolean
LANGUAGE sql SECURITY DEFINER SET search_path=pg_catalog,pg_temp AS $$
 WITH changed AS (UPDATE public.user_mfa SET sign_count=new_count,public_key=pkey
 WHERE id=mid AND user_id=uid AND kind='passkey' AND enabled_at IS NOT NULL
 AND sign_count=old_count AND (new_count=0 OR new_count>old_count) RETURNING id)
 SELECT EXISTS(SELECT FROM changed);
$$;
CREATE FUNCTION user_mfa_remove(mid uuid, uid uuid, sid uuid) RETURNS boolean
LANGUAGE sql SECURITY DEFINER SET search_path=pg_catalog,pg_temp AS $$
 WITH changed AS (DELETE FROM public.user_mfa WHERE id=mid AND user_id=uid
 AND EXISTS(SELECT FROM public.sessions WHERE id=sid AND user_id=uid
 AND reauth_at>clock_timestamp()-interval '5 minutes' AND reauth_at<=clock_timestamp()
 AND expires_at>clock_timestamp()) RETURNING id)
 SELECT EXISTS(SELECT FROM changed);
$$;
GRANT CREATE ON SCHEMA public TO yhotos_admin;
ALTER FUNCTION user_mfa_list(uuid) OWNER TO yhotos_admin;
ALTER FUNCTION user_mfa_add(uuid,uuid,text,bytea,bytea,bytea,bigint,bigint) OWNER TO yhotos_admin;
ALTER FUNCTION user_mfa_use(uuid,uuid,bigint) OWNER TO yhotos_admin;
ALTER FUNCTION user_passkey_use(uuid,uuid,bigint,bigint,bytea) OWNER TO yhotos_admin;
ALTER FUNCTION user_mfa_remove(uuid,uuid,uuid) OWNER TO yhotos_admin;
REVOKE CREATE ON SCHEMA public FROM yhotos_admin;
REVOKE ALL ON FUNCTION user_mfa_list(uuid),user_mfa_add(uuid,uuid,text,bytea,bytea,bytea,bigint,bigint),user_mfa_use(uuid,uuid,bigint),user_passkey_use(uuid,uuid,bigint,bigint,bytea),user_mfa_remove(uuid,uuid,uuid) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION user_mfa_list(uuid),user_mfa_add(uuid,uuid,text,bytea,bytea,bytea,bigint,bigint),user_mfa_use(uuid,uuid,bigint),user_passkey_use(uuid,uuid,bigint,bigint,bytea),user_mfa_remove(uuid,uuid,uuid) TO yhotos_public,yhotos_admin;

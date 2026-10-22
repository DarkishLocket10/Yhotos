-- Counterless authenticators may remain at zero; a counted credential cannot reset.
-- Preserve the compare-and-set so overlapping ceremonies cannot overwrite newer state.
CREATE OR REPLACE FUNCTION user_passkey_use(mid uuid, uid uuid, old_count bigint, new_count bigint, pkey bytea) RETURNS boolean
LANGUAGE sql SECURITY DEFINER SET search_path=pg_catalog,pg_temp AS $$
 WITH changed AS (UPDATE public.user_mfa SET sign_count=new_count,public_key=pkey
 WHERE id=mid AND user_id=uid AND kind='passkey' AND enabled_at IS NOT NULL
 AND sign_count=old_count AND ((old_count=0 AND new_count=0) OR new_count>old_count) RETURNING id)
 SELECT EXISTS(SELECT FROM changed);
$$;

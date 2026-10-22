-- Bulk inserts need database-generated ids without abandoning the UUIDv7 model.
-- RFC 9562: 48-bit Unix milliseconds, version 7, and 74 random bits. The variant
-- bits come from gen_random_uuid(); only its version nibble is replaced.
CREATE FUNCTION yhotos_uuid_v7() RETURNS uuid
LANGUAGE sql VOLATILE SET search_path = pg_catalog, pg_temp AS $$
 WITH raw AS (
  SELECT substring(int8send(floor(extract(epoch FROM clock_timestamp())*1000)::bigint) FROM 3)
   || substring(uuid_send(gen_random_uuid()) FROM 7) AS bytes
 ) SELECT encode(set_byte(bytes,6,(get_byte(bytes,6)&15)|112),'hex')::uuid FROM raw
$$;
REVOKE ALL ON FUNCTION yhotos_uuid_v7() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION yhotos_uuid_v7() TO yhotos_admin, yhotos_worker;

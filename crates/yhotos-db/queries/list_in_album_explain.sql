EXPLAIN (ANALYZE, FORMAT TEXT) SELECT p.id as "id: _", p.created_at, p.updated_at, p.short_id as "short_id: _", p.album_id as "album_id: _", p.title, p.description, p.taken_at, p.posted_at, p.sort_order, p.visibility as "visibility: _", p.original_hash, p.original_name, p.original_bytes, p.mime, p.kind as "kind: _", p.width, p.height, p.orientation, p.is_hdr, p.has_gain_map, p.icc_name, p.camera_make, p.camera_model, p.lens, p.focal_mm, p.aperture, p.shutter, p.iso, p.gps_lat, p.gps_lon, p.place_id, p.dominant_colors, p.placeholder, p.deleted_at,
      coalesce((SELECT jsonb_agg(jsonb_build_object('variant',d.variant,'format',d.format,'hdr',d.hdr,'hash',d.hash,'width',d.width,'height',d.height,'bytes',d.bytes) ORDER BY d.variant,d.format,d.hdr)
        FROM derivatives d WHERE d.photo_id=p.id), '[]'::jsonb) AS "derivatives!"
      FROM photos p WHERE p.album_id=$1 AND p.deleted_at IS NULL
      ORDER BY CASE WHEN $2='manual' THEN p.sort_order END,
               CASE WHEN $2='taken' THEN p.taken_at END DESC NULLS LAST,
               CASE WHEN $2='posted' THEN p.posted_at END DESC,
               CASE WHEN $2='name' THEN p.title END, p.id LIMIT $3 OFFSET $4

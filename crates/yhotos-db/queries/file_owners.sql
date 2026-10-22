WITH RECURSIVE selected AS (
    SELECT p.id AS photo_id, p.original_bytes AS bytes, p.mime, p.width, p.height
    FROM photos p WHERE $2 = 'original' AND p.original_hash = $1
    UNION ALL
    SELECT d.photo_id, d.bytes,
           CASE d.format WHEN 'avif' THEN 'image/avif' WHEN 'jxl' THEN 'image/jxl'
                         WHEN 'jpeg' THEN 'image/jpeg' END,
           d.width, d.height
    FROM derivatives d WHERE $2 = 'derivative' AND d.hash = $1
        AND d.variant = $3 AND d.format = $4 AND d.hdr = $5
    UNION ALL
    SELECT v.photo_id, v.bytes, 'video/mp4', v.width, v.height
    FROM video_renditions v WHERE $2 = 'video' AND v.hash = $1 AND NOT v.hls
), chain AS (
    SELECT s.photo_id, a.id, a.parent_id, a.visibility, a.deleted_at,
           0 AS depth, ARRAY[a.id] AS visited
    FROM selected s JOIN photos p ON p.id = s.photo_id JOIN albums a ON a.id = p.album_id
    UNION ALL
    SELECT c.photo_id, a.id, a.parent_id, a.visibility, a.deleted_at,
           c.depth + 1, c.visited || a.id
    FROM chain c JOIN albums a ON a.id = c.parent_id WHERE NOT a.id = ANY(c.visited)
)
SELECT p.id AS "photo_id!: _", p.visibility AS "photo_visibility!: _",
       p.original_name AS "original_name!", p.kind AS "kind!: _",
       s.bytes AS "bytes!", s.mime AS "mime!", s.width, s.height,
       ARRAY(SELECT principal_id FROM photo_grants WHERE photo_id = p.id AND principal_kind = 'user') AS "photo_users!",
       ARRAY(SELECT principal_id FROM photo_grants WHERE photo_id = p.id AND principal_kind = 'group') AS "photo_groups!",
       (SELECT jsonb_agg(jsonb_build_object(
           'id', c.id, 'visibility', c.visibility,
           'users', ARRAY(SELECT principal_id FROM album_grants WHERE album_id = c.id AND principal_kind = 'user'),
           'groups', ARRAY(SELECT principal_id FROM album_grants WHERE album_id = c.id AND principal_kind = 'group')
       ) ORDER BY c.depth) FROM chain c WHERE c.photo_id = p.id) AS "albums!"
FROM selected s JOIN photos p ON p.id = s.photo_id
WHERE p.deleted_at IS NULL
  AND EXISTS(SELECT 1 FROM chain c WHERE c.photo_id = p.id AND c.parent_id IS NULL)
  AND NOT EXISTS(SELECT 1 FROM chain c WHERE c.photo_id = p.id AND c.deleted_at IS NOT NULL)
ORDER BY p.id

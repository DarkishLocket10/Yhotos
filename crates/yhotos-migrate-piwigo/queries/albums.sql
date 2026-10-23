SELECT CAST(id AS SIGNED) AS `id: i64`, CAST(id_uppercat AS SIGNED) AS `parent: i64`,
 name, comment AS description, global_rank, CAST(representative_picture_id AS SIGNED) AS `cover: i64`,
 status, permalink AS url_name
FROM piwigo_categories ORDER BY id

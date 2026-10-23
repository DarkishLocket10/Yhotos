SELECT CAST(id AS SIGNED) AS `id: i64`, CAST(id_uppercat AS SIGNED) AS `parent: i64`,
 name, comment AS description, global_rank, CAST(representative_picture_id AS SIGNED) AS `cover: i64`,
 status, visible, permalink AS url_name,
 CAST(EXISTS(SELECT 1 FROM piwigo_user_access u WHERE u.cat_id=c.id) AS SIGNED) AS `user_access!: i64`,
 CAST(EXISTS(SELECT 1 FROM piwigo_group_access g WHERE g.cat_id=c.id) AS SIGNED) AS `group_access!: i64`
FROM piwigo_categories c ORDER BY id

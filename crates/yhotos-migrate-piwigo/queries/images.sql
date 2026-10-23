SELECT CAST(id AS SIGNED) AS `id: i64`, file AS filename, path, name, comment AS description,
 author, date_creation AS taken, date_available AS posted,
 CAST(level AS SIGNED) AS `level: i64`, latitude AS lat, longitude AS lon,
 CAST(width AS SIGNED) AS `width: i64`, CAST(height AS SIGNED) AS `height: i64`
FROM piwigo_images ORDER BY id

SELECT CAST(image_id AS SIGNED) AS `image: i64`, CAST(category_id AS SIGNED) AS `category: i64`,
 CAST(rank AS SIGNED) AS `photo_order: i64` FROM piwigo_image_category ORDER BY image_id,category_id

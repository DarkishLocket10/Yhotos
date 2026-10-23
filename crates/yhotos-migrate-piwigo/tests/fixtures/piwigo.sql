-- Synthetic Piwigo 16 schema shapes; no real gallery metadata or credentials.
DROP TABLE IF EXISTS piwigo_image_tag,piwigo_image_category,piwigo_tags,piwigo_images,piwigo_categories,piwigo_config;
CREATE TABLE piwigo_config (param varchar(40) NOT NULL PRIMARY KEY, value text) ENGINE=MyISAM;
CREATE TABLE piwigo_categories (
 id mediumint unsigned NOT NULL PRIMARY KEY, id_uppercat mediumint unsigned DEFAULT NULL,
 name varchar(255) NOT NULL, comment text, global_rank varchar(255),
 representative_picture_id mediumint unsigned DEFAULT NULL,
 status enum('public','private') NOT NULL DEFAULT 'public', permalink varchar(64)
) ENGINE=MyISAM;
CREATE TABLE piwigo_images (
 id mediumint unsigned NOT NULL PRIMARY KEY, file varchar(255) NOT NULL, path varchar(255) NOT NULL,
 name varchar(255), comment text, author varchar(255), date_creation datetime,
 date_available datetime NOT NULL, level tinyint unsigned NOT NULL DEFAULT 0,
 latitude double, longitude double, width mediumint unsigned, height mediumint unsigned
) ENGINE=MyISAM;
CREATE TABLE piwigo_image_category (
 image_id mediumint unsigned NOT NULL, category_id mediumint unsigned NOT NULL,
 rank mediumint unsigned DEFAULT NULL, PRIMARY KEY(image_id,category_id)
) ENGINE=MyISAM;
CREATE TABLE piwigo_tags (id smallint unsigned NOT NULL PRIMARY KEY,name varchar(255) NOT NULL,url_name varchar(255)) ENGINE=MyISAM;
CREATE TABLE piwigo_image_tag (image_id mediumint unsigned NOT NULL,tag_id smallint unsigned NOT NULL,PRIMARY KEY(image_id,tag_id)) ENGINE=MyISAM;
INSERT INTO piwigo_config VALUES ('piwigo_db_version','16.0.0');
INSERT INTO piwigo_categories VALUES
 (1,NULL,'Snow &amp; Ice','<p>A &lt;b&gt;trip&lt;/b&gt;</p>','2',1,'public','snow_ice'),
 (2,1,'Nested','<script>text only</script>','2.1',1,'public',''),
 (3,NULL,'Snow &amp; Ice',NULL,'10',2,'private',NULL);
INSERT INTO piwigo_images VALUES
 (1,'one.jpg','./upload/2024/one.jpg','Ice &amp; snow','<p>A photo</p>','A &amp; B','2024-01-02 03:04:05','2024-02-03 04:05:06',0,45.5,-73.6,100,80),
 (2,'two.jpg','upload/2024/two.jpg',NULL,NULL,'Photographer',NULL,'2024-02-03 04:05:06',1,NULL,NULL,100,80),
 (3,'missing.jpg','2024/missing.jpg','Missing',NULL,NULL,NULL,'2024-02-03 04:05:06',0,NULL,NULL,NULL,NULL),
 (4,'unsafe.jpg','../outside.jpg','Unsafe',NULL,NULL,NULL,'2024-02-03 04:05:06',0,NULL,NULL,NULL,NULL),
 (5,'five.png','2024/five.png',NULL,NULL,NULL,NULL,'2024-02-03 04:05:06',0,999,1,100,80),
 (6,'orphan.jpg','2024/orphan.jpg','Orphan',NULL,NULL,NULL,'2024-02-03 04:05:06',0,NULL,NULL,NULL,NULL);
INSERT INTO piwigo_image_category VALUES (1,1,8),(1,2,3),(2,3,1),(3,1,2),(4,1,3),(5,2,4);
INSERT INTO piwigo_tags VALUES (1,'Best &amp; Bright','best_bright'),(2,'Best &amp; Bright','best_bright');
INSERT INTO piwigo_image_tag VALUES (1,1),(2,2);

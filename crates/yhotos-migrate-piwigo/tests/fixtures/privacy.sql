-- Additional Piwigo access combinations, layered on the base fixture.
INSERT INTO piwigo_categories VALUES
 (10,NULL,'Locked',NULL,'30',10,'public','locked','false'),
 (11,NULL,'User private',NULL,'31',11,'private','user_private','true'),
 (12,NULL,'Group private',NULL,'32',12,'private','group_private','true'),
 (13,NULL,'Level public',NULL,'33',13,'public','level_public','true'),
 (14,10,'Locked child',NULL,'30.1',14,'public','locked_child','true'),
 (15,NULL,'User access',NULL,'34',15,'public','user_access','true'),
 (16,NULL,'Group access',NULL,'35',16,'public','group_access','true'),
 (17,NULL,'Locked private',NULL,'36',17,'private','locked_private','false');
INSERT INTO piwigo_user_access VALUES (100,11),(100,15),(100,17);
INSERT INTO piwigo_group_access VALUES (200,12),(200,16),(200,17);
INSERT INTO piwigo_images (id,file,path,date_available,level) VALUES
 (10,'10.jpg','2024/10.jpg','2024-01-01',0),
 (11,'11.jpg','2024/11.jpg','2024-01-01',0),
 (12,'12.jpg','2024/12.jpg','2024-01-01',0),
 (13,'13.jpg','2024/13.jpg','2024-01-01',8),
 (14,'14.jpg','2024/14.jpg','2024-01-01',0),
 (15,'15.jpg','2024/15.jpg','2024-01-01',0),
 (16,'16.jpg','2024/16.jpg','2024-01-01',0),
 (17,'17.jpg','2024/17.jpg','2024-01-01',0);
INSERT INTO piwigo_image_category VALUES
 (10,10,1),(11,11,1),(12,12,1),(13,13,1),
 (14,14,1),(15,15,1),(16,16,1),(17,17,1);

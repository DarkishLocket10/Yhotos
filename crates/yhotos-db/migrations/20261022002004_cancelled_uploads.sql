-- Purge cancels uploads transactionally; workers unlink their temporary files after commit.
-- No FK: this cleanup root must survive deletion of its photo, album, and upload.
CREATE TABLE upload_gc (
 tmp_path text PRIMARY KEY CHECK (tmp_path ~ '^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$')
);
GRANT ALL ON upload_gc TO yhotos_admin, yhotos_worker;

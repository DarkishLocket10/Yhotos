-- Only ingest publication may restore source overrides. Administrative metadata
-- edits (including NULL removal) must remain authoritative after cutover.
DROP TRIGGER preserve_piwigo_metadata ON photos;
CREATE TRIGGER preserve_piwigo_metadata
 BEFORE UPDATE OF derivative_plan ON photos
 FOR EACH ROW WHEN (NEW.status = 'ready' AND NEW.derivative_plan IS NOT NULL)
 EXECUTE FUNCTION preserve_piwigo_metadata();

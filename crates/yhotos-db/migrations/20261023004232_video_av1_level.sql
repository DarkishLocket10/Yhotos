-- NULL keeps existing renditions usable through browser container sniffing.
-- New AV1 publications record the confined probe's actual sequence level index.
ALTER TABLE video_renditions ADD COLUMN av1_level smallint
    CHECK (av1_level IS NULL OR (av1_level BETWEEN 0 AND 23 AND codec = 'av1'));

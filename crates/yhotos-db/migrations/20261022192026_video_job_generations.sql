-- Task 007 queued placeholder video jobs using only photo_id. Upgrade those
-- unfinished envelopes before task 021's generation-fenced handler claims them.
UPDATE jobs AS j
SET payload = jsonb_build_object(
    'photo_id', p.id,
    'original_hash', p.original_hash,
    'generation', p.generation
)
FROM photos AS p
WHERE j.kind = 'video.transcode'
  AND j.finished_at IS NULL
  AND j.payload = jsonb_build_object('photo_id', p.id);

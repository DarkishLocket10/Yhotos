-- Repeated flags are checked under an advisory transaction lock by the HTTP API.
-- These indexes bound the actor lookup without changing existing flag history.
CREATE INDEX flags_photo_user ON flags (photo_id, user_id) WHERE user_id IS NOT NULL;
CREATE INDEX flags_photo_link ON flags (photo_id, share_link_id) WHERE share_link_id IS NOT NULL;

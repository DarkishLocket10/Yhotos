-- Content URLs look up by representation digest, rather than photo_id.
CREATE INDEX derivatives_hash_lookup ON derivatives(hash, variant, format, hdr);
CREATE INDEX video_hash_lookup ON video_renditions(hash, codec) WHERE NOT hls;

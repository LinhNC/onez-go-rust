CREATE INDEX IF NOT EXISTS links_status_updated_id_idx
ON links(status, updated_at DESC, id DESC);

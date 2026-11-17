CREATE TABLE IF NOT EXISTS links (
  id INTEGER PRIMARY KEY,
  slug TEXT NOT NULL COLLATE BINARY UNIQUE,
  destination_url TEXT NOT NULL,
  title TEXT NOT NULL DEFAULT '',
  notes TEXT NOT NULL DEFAULT '',
  status TEXT NOT NULL CHECK (status IN ('active', 'disabled')),
  redirect_status INTEGER NOT NULL DEFAULT 302 CHECK (redirect_status IN (301, 302, 307, 308)),
  expires_at INTEGER NULL,
  visit_count INTEGER NOT NULL DEFAULT 0 CHECK (visit_count >= 0),
  last_visited_at INTEGER NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS links_created_id_idx ON links(created_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS links_updated_id_idx ON links(updated_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS links_visits_id_idx ON links(visit_count DESC, id DESC);
CREATE INDEX IF NOT EXISTS links_destination_id_idx ON links(destination_url COLLATE NOCASE, id);
CREATE INDEX IF NOT EXISTS links_status_expiry_idx ON links(status, expires_at, id);

CREATE VIRTUAL TABLE IF NOT EXISTS links_fts USING fts5(
  slug,
  destination_url,
  title,
  notes,
  content='links',
  content_rowid='id',
  tokenize='unicode61'
);

CREATE TRIGGER IF NOT EXISTS links_fts_insert AFTER INSERT ON links BEGIN
  INSERT INTO links_fts(rowid, slug, destination_url, title, notes)
  VALUES (new.id, new.slug, new.destination_url, new.title, new.notes);
END;

CREATE TRIGGER IF NOT EXISTS links_fts_delete AFTER DELETE ON links BEGIN
  INSERT INTO links_fts(links_fts, rowid, slug, destination_url, title, notes)
  VALUES ('delete', old.id, old.slug, old.destination_url, old.title, old.notes);
END;

CREATE TRIGGER IF NOT EXISTS links_fts_update AFTER UPDATE OF slug, destination_url, title, notes ON links BEGIN
  INSERT INTO links_fts(links_fts, rowid, slug, destination_url, title, notes)
  VALUES ('delete', old.id, old.slug, old.destination_url, old.title, old.notes);
  INSERT INTO links_fts(rowid, slug, destination_url, title, notes)
  VALUES (new.id, new.slug, new.destination_url, new.title, new.notes);
END;

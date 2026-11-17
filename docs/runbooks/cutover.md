# KV to SQLite cutover and rollback

## Preconditions

- A final KV export adapter produces the canonical NDJSON described in `docs/kv-migration.md`.
- Staging has passed API replay, public URL checks, restore verification and the reference benchmark.
- The Rust service runs as one active writer on a persistent local SSD/NVMe volume.
- The final KV export, importer reports and a verified SQLite backup are retained outside the service volume.

## Cutover

1. Announce the maintenance window and block Worker create/update/delete operations while keeping redirects readable.
2. Take the final KV export and hash the source file.
3. Run the importer dry run with `-conflict abort`; resolve every rejected or conflicting row.
4. Run committed import with `-verify`; retain the machine-readable report and checksum.
5. Create an online backup with `cargo run -p shorten-cli --bin backup -- -database .data/shorten.db -output .data/backups/shorten.db` and verify its manifest.
6. Start exactly one Rust service instance, wait for `/readyz`, and replay representative API and public URLs.
7. Switch ingress to the Rust service. Do not delete the Worker deployment or KV namespace.
8. Monitor redirect outcomes/latency, cache hit rate, analytics backlog, SQLite busy errors, disk/WAL growth, and management mutations.
9. After the observation window, disable the old Worker mutation code and archive KV/export artifacts according to retention policy.

## Rollback

1. Stop new Rust mutations and record the rollback time.
2. Export all SQLite links created or changed after cutover for reconciliation; do not assume KV contains them.
3. Route traffic back to the frozen Worker/KV snapshot.
4. Reconcile post-cutover mutations into the selected source of truth before reopening management writes.
5. Record the cause, data reconciliation result and next cutover criteria.

Rollback is an operator-controlled maintenance action. The service intentionally does not dual-write SQLite and KV.

# Go service to Rust service

The Rust rewrite reads and writes the same SQLite schema, migrations, timestamp encoding and cursor format, so the volume carries over unchanged. Only the process swaps.

1. Take an online backup with the Go tool (`go run -tags sqlite_fts5 ./cmd/backup -database <path> -output <path>`) and verify its manifest.
2. Restore that backup file into a staging Rust deployment and confirm the link and visit totals reported by `backup` match, that `schema_migrations` is unchanged, and that `/readyz` reports ready.
3. Record the staging replay results: API responses, public redirects, the management list orders and the login flow.
4. Stop the Go process, which drains requests, flushes analytics and checkpoints the WAL. Confirm the process has exited before starting the second writer.
5. Start one Rust process against the same volume with the same `.env`, then wait for `/readyz` and replay the same requests.
6. Keep the previous Go binary available for the observation window so the process can be rolled back to it without touching the database.

Never run the Go and Rust services simultaneously against one volume: SQLite has one writer.
Rollback is a process swap in the same direction, not a schema conversion, because the file
format is identical.

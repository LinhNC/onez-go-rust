# Cloudflare KV migration

The importer consumes newline-delimited JSON. Each line contains a KV `key` and `value`. `value` may be the link object itself or the JSON string returned by a bulk KV export adapter.

```json
{"key":"link:article","value":{"url":"https://example.com/article","createdAt":"2026-07-01T10:00:00Z","visits":42,"lastVisitedAt":"2026-08-01T08:30:00Z"}}
```

Only `link:` keys are imported. Required source fields are `url` and a valid RFC3339 `createdAt`; `visits` defaults to zero and `lastVisitedAt` defaults to null. The importer applies the service's slug and URL validation.

Run a dry run first; dry run is the default and does not write logical database state:

```sh
cargo run -p shorten-cli --bin kvimport \
  -input export.ndjson \
  -database .data/shorten.db
```

Commit and verify:

```sh
cargo run -p shorten-cli --bin kvimport \
  -input export.ndjson \
  -database .data/shorten.db \
  -commit -verify -conflict abort
```

Flags: `-input` defaults to `-` (stdin), `-batch` is 500 records per transaction and `-conflict` defaults to `abort`. `-verify` compares accepted source records against SQLite and exits non-zero on a count or content mismatch, so it needs a file input.

The repository ships a small sample export for smoke tests, including a non-`link:` key that the importer must ignore:

```sh
cargo run -p shorten-cli --bin kvimport \
  -input testdata/kv-export.ndjson \
  -database .data/shorten.db
```

Conflict policies:

- `abort` skips identical rows but rolls back the current batch on a different existing row.
- `skip` leaves different existing rows unchanged.
- `replace` atomically replaces lifecycle and analytics fields from the export.

The JSON report records input, imported, identical, ignored, rejected, conflicting, replaced and skipped totals plus an order-independent logical checksum. Retain the dry-run and commit reports with the final export.

The migration runs against the same database schema the Go service wrote. Import into a copy of the live database first, confirm the report and checksum, then schedule the cutover described in [docs/runbooks/cutover.md](runbooks/cutover.md).

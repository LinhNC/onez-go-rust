# Shorten

A fast, self-hosted URL shortener and management console for one administrator. Shorten uses Rust, Axum, Askama, HTMX and SQLite to keep public redirects quick while providing complete link lifecycle management.

## Highlights

- Create generated or custom short codes.
- Configure 301, 302, 307 and 308 redirects.
- Search, filter, sort, edit, rename, disable, expire and delete links.
- Preview and bulk-delete links older than 3, 6 or 12 months.
- Track successful GET redirects with batched, concurrency-safe analytics.
- Use the server-rendered UI with or without JavaScript.
- Call the JSON API directly with `X-API-Key` authentication.
- Import existing Cloudflare KV records.
- Run health checks, Prometheus metrics, backups and reproducible benchmarks.

> Shorten uses one SQLite writer. Run exactly one application replica for each database volume.

## Deploy with Docker Compose

For a VPS with a host-level Caddy or Nginx installation:

```sh
cp .env.example .env
openssl rand -hex 32
openssl rand -hex 48
```

Put the generated secrets and the exact public HTTPS origin into `.env`, then start the service:

```sh
docker compose build --pull
docker compose up -d
docker compose ps
curl -fsS http://127.0.0.1:8080/readyz
```

The standard Compose file binds to `127.0.0.1:8080`. Keep that port closed in the firewall and proxy HTTPS traffic to it.

Minimal Caddy configuration:

```caddyfile
go.example.com {
	@metrics path /metrics
	respond @metrics 404
	reverse_proxy 127.0.0.1:8080
}
```

Update an existing deployment with:

```sh
git pull --ff-only
docker compose up -d --build --remove-orphans
docker compose logs -f --tail=200 shorten
```

SQLite is stored in the `shorten_data` volume. Normal redeploys preserve it, but deleting the volume permanently removes all links and analytics.

- Keep the replica count at one.
- Include `shorten_data` in the VPS backup plan.
- Do not store the live database on object storage or an unsafe network filesystem.
- Test restoration regularly instead of assuming a backup is usable.

## Configuration

Durations accept the same strings the Go service accepted: `250ms`, `5m`, `12h`, `1.5h`.

### Required in production

| Variable | Purpose |
| --- | --- |
| `SHORTEN_PUBLIC_BASE_URL` | Public HTTPS origin used to generate short URLs and validate browser origins. |
| `SHORTEN_ADMIN_API_KEY` | Administrator credential for browser login and JSON API requests; minimum 16 characters. |
| `SHORTEN_SESSION_SECRET` | Signs browser sessions; minimum 32 characters and must differ from the API key. |

### Optional tuning

| Variable | Default | Purpose |
| --- | ---: | --- |
| `SHORTEN_ADDRESS` | `:8080` | Listen address. |
| `SHORTEN_DATABASE_PATH` | `.data/shorten.db` | SQLite database path. |
| `SHORTEN_PREVIOUS_SESSION_SECRET` | Empty | Previous signing secret during key rotation. |
| `SHORTEN_INSECURE_COOKIES` | `false` | Drop the `Secure` cookie flag; use only for local HTTP development. |
| `SHORTEN_ALLOW_NULL_ORIGIN` | `false` | Permit browser requests that send `Origin: null`; enable only when a trusted browser or webview requires it. |
| `SHORTEN_READ_CONNECTIONS` | CPU-based | Maximum SQLite read connections. Start with `4` on a small VPS. |
| `SHORTEN_CACHE_CAPACITY` | `10000` | Maximum redirect cache entries. |
| `SHORTEN_CACHE_POSITIVE_TTL` | `5m` | Cache lifetime for existing slugs. |
| `SHORTEN_CACHE_NEGATIVE_TTL` | `10s` | Cache lifetime for missing slugs. |
| `SHORTEN_ANALYTICS_INTERVAL` | `250ms` | Visit-counter flush interval. |
| `SHORTEN_ANALYTICS_THRESHOLD` | `1000` | Unique pending links that trigger an early flush. |
| `SHORTEN_ANALYTICS_MAX_KEYS` | `50000` | Maximum pending analytics cardinality. |
| `SHORTEN_SESSION_TTL` | `12h` | Browser session lifetime. |
| `SHORTEN_SHUTDOWN_TIMEOUT` | `15s` | Deadline for request draining, analytics flush and WAL checkpoint. |

Compose configures `SHORTEN_ADDRESS`, `SHORTEN_DATABASE_PATH` and secure cookies. Use `SHORTEN_INSECURE_COOKIES=true` only for local HTTP development.

## Management UI

Open `/manage/login` and authenticate with the administrator API key.

The UI supports:

- Creating a destination with an optional custom short code.
- Editing metadata, status, expiration and redirect behavior.
- Renaming a short code through a separately confirmed workflow.
- Searching and filtering large link collections with keyset pagination.
- Permanently deleting individual links.
- Reviewing and cleaning old links in bulk.

### Cleaning old links

Open **Links → Cleanup old links**.

1. Choose links older than 3, 6 or 12 months.
2. Choose which states are eligible.
3. Review the exact count and oldest matching records.
4. Confirm the warning before deleting.

Age is calculated from the link creation date. The safe default selects only links that are disabled or expired; active links are included only when **All statuses, including active** is selected explicitly.

Bulk deletion is permanent. It removes the link, its search entry and cached redirect state. Create a verified backup before cleaning a large dataset.

## JSON API

All management API routes require:

```http
X-API-Key: YOUR_ADMIN_API_KEY
```

| Method | Route | Purpose |
| --- | --- | --- |
| `POST` | `/api/shorten` | Create a short link. |
| `GET` | `/api/links` | List links with a nullable cursor. |
| `GET` | `/api/links/{slug}` | Retrieve complete link details. |
| `PATCH` | `/api/links/{slug}` | Update lifecycle fields or rename the slug. |
| `DELETE` | `/api/links/{slug}` | Permanently delete one link. |

Set reusable shell values:

```sh
export SHORTEN_URL=https://go.example.com
export SHORTEN_KEY=your-admin-api-key
```

Create a link:

```sh
curl -X POST "$SHORTEN_URL/api/shorten" \
  -H 'Content-Type: application/json' \
  -H "X-API-Key: $SHORTEN_KEY" \
  -d '{"url":"https://example.com/article","slug":"article"}'
```

List and update links:

```sh
curl -H "X-API-Key: $SHORTEN_KEY" "$SHORTEN_URL/api/links?limit=30"

curl -X PATCH "$SHORTEN_URL/api/links/article" \
  -H 'Content-Type: application/json' \
  -H "X-API-Key: $SHORTEN_KEY" \
  -d '{"status":"disabled"}'
```

Delete one link:

```sh
curl -X DELETE \
  -H "X-API-Key: $SHORTEN_KEY" \
  "$SHORTEN_URL/api/links/article"
```

Public `GET /{slug}` and `HEAD /{slug}` redirects do not require authentication.

## Local development

Requirements: a stable Rust toolchain (cargo, 1.85 or newer) and a C compiler for the vendored SQLite build. Node.js 22+ and pnpm are needed only to rebuild the design-system stylesheet (`frontend/package.json` pins the pnpm version through `packageManager`); the generated CSS is committed, so `cargo build` needs no npm or pnpm.

```sh
cp .env.example .env
set -a
. ./.env
set +a
make build
make test
make run
```

Open `http://localhost:8080/manage/login`.

Useful commands:

| Command | Purpose |
| --- | --- |
| `make frontend` | Rebuild the Astro + shadcn design system into the embedded stylesheet. |
| `make build` | Build the whole workspace. |
| `make run` | Run the server (`cargo run -p shorten-server --bin shorten`). |
| `make test` | Run the full test suite. |
| `make lint` | Clippy over all targets with warnings denied. |
| `make format` | `cargo fmt --all`. |
| `make verify` | Format check, clippy, tests and the stylesheet freshness check. |
| `make test-sanitizer` | Optional nightly thread-sanitizer run; Rust has no built-in race detector. |

## Operations

### Health and metrics

| Route | Purpose |
| --- | --- |
| `/healthz` | Process liveness. |
| `/readyz` | SQLite, migrations and analytics readiness. |
| `/metrics` | Prometheus-format operational metrics. Restrict this route at the proxy when it is not intentionally public. |

### Command-line tools

Built with cargo, installed under `target/release/`:

| Binary | Flags | Purpose |
| --- | --- | --- |
| `shorten` | none (configuration comes from the environment) | Run the server. |
| `backup` | `-database`, `-output` | Online SQLite backup with integrity check and manifest. |
| `kvimport` | `-input`, `-database`, `-commit`, `-conflict`, `-batch`, `-verify` | Import a Cloudflare KV NDJSON export. |
| `seed` | `-database`, `-count`, `-batch` | Generate deterministic links for benchmarking. |
| `bench` | `-target`, `-duration`, `-concurrency` | Warmed redirect load harness. |
| `querybench` | `-base-url`, `-api-key`, `-iterations` | Management query latency harness. |
| `healthcheck` | none | Probe `http://127.0.0.1:8080/readyz`, honouring `SHORTEN_HEALTHCHECK_URL`. |

Run any of them through cargo, for example:

```sh
cargo run -p shorten-cli --bin backup -- -database .data/shorten.db -output .data/backups/shorten.db
```

Long options accept both `-flag value` and `--flag value`.

### Backup

Create an application-consistent online SQLite backup:

```sh
cargo run -p shorten-cli --bin backup -- \
  -database .data/shorten.db \
  -output .data/backups/shorten.db
```

The command verifies integrity and writes a manifest containing schema, link and visit totals. Never copy a live SQLite database file directly. If the deployment platform only supports raw volume snapshots, stop the application first and verify the restored database separately.

In a Compose deployment the same tool ships in the image:

```sh
docker compose run --rm --entrypoint backup shorten \
  -database /data/shorten.db \
  -output /data/backups/shorten.db
```

### Cloudflare KV migration

See [docs/kv-migration.md](docs/kv-migration.md) for the NDJSON format, dry run, conflict policies, committed import and checksum verification.

### Performance

The reference profile uses one million seeded links, 200 keep-alive clients and a 60-second warmed redirect run. See [docs/benchmark.md](docs/benchmark.md) for budgets, commands and recorded local evidence.

## Architecture and limits

- Axum on the Tokio runtime serves redirects, the management UI, JSON API and operational endpoints.
- Askama renders complete pages and HTMX fragments from the same handlers.
- SQLite runs in WAL mode with one write connection and a bounded read pool (`rusqlite` behind a connection pool).
- A bounded LRU cache keeps hot redirects off the database path.
- Visit analytics are accumulated in memory and flushed additively in batches.
- Graceful shutdown drains HTTP requests, flushes analytics and checkpoints the WAL.
- The design-system stylesheet is embedded in the binary and served from `GET /assets/styles.css` with immutable cache headers.

SQLite is intentionally a single-writer deployment. Do not scale multiple active replicas against the same database file.

## Security

- The API key is compared in constant time and is never stored in browser storage.
- Browser sessions are signed, expiring, Secure, HttpOnly and SameSite=Strict.
- Browser mutations require a session-bound CSRF token.
- Login and mutation endpoints use bounded per-IP rate limits.
- Responses include a restrictive Content Security Policy and other security headers.
- The production container runs as UID/GID 10001 with a read-only root filesystem and dropped Linux capabilities.

## Rust migration notes

This is a Rust rewrite of the Go service. The product, routes, JSON API, cookie
and header behaviour, configuration variables, SQLite schema and command-line
tools are unchanged; only the implementation technology moved.

- `net/http` and `encoding/json` → Axum, Hyper and Tokio, with `serde` / `serde_json` payloads.
- templ components → Askama templates, rendered server-side with the same HTMX fragment contract.
- `mattn/go-sqlite3` with CGO and the `sqlite_fts5` build tag → `rusqlite` with the `bundled`
  feature, which vendors and compiles SQLite with FTS5 enabled. No CGO, no Go toolchain, no
  build tag: a C compiler is the only native requirement.
- The Go concept stylesheet → the Astro + shadcn design system in `frontend/`, which builds the
  committed stylesheet embedded into the web crate.

Data compatibility is preserved. The SQLite schema and the `schema_migrations`
table and migration files are the same, timestamps are stored the same way
(whole milliseconds, UTC) and cursors keep the same encoding (base64url-encoded
JSON, cursor `version: 1`, link ID tie-breaker). A database produced by the Go
service opens unchanged: run the migration set against a copy of the live file,
confirm `schema_migrations` is unchanged and links and visit counts match, then
hand the file to the Rust service. The Go service's `backup` output restores
into the Rust service without transformation.

## Additional documentation

- [Go parity contract](docs/parity.md)
- [Cloudflare KV migration](docs/kv-migration.md)
- [Performance benchmark](docs/benchmark.md)
- [Production cutover and rollback](docs/runbooks/cutover.md)

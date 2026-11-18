# Go parity contract

This Rust service replaces the Go implementation of Shorten. It is a port, not a
redesign: the wire behaviour is the contract, and the contract is verified by
tests (`crates/shorten/tests`, `crates/shorten-web/tests`,
`crates/shorten-cli/tests`) and by live checks against the running binary.

## What must match, byte for byte

| Surface | Contract |
| --- | --- |
| Route table | Every path, method and redirect target of `internal/web/server.go`, including `GET /manage/cleanup`, `POST /manage/links/{slug}/rename`, `POST /manage/links/{slug}/delete`, `HEAD /{slug}`. Unmatched paths answer `404 page not found\n` with the security headers, and the same paths are labelled with Go's `normalizedRoute` rules in `/metrics`. |
| Security headers | `x-request-id` (base36 counter starting at 1), `x-content-type-options: nosniff`, `referrer-policy: no-referrer`, `permissions-policy: camera=(), microphone=(), geolocation=()`, the exact CSP string. Set on every response, including 404s. No `Cache-Control` on HTML, redirects, API or metrics responses. |
| Redirects | Status comes from the stored `redirect_status` (301/302/307/308); `Location` is the stored destination and the body is `http.Redirect`'s one-liner `<a href="…">Moved Permanently</a>` / `Found` / `Temporary Redirect` / `Permanent Redirect` with `Content-Type: text/html; charset=utf-8`. Disabled and expired links answer 410 with Go's body. Any other method on `/{slug}` is the handler's own 405: `Allow: GET, HEAD` and the body `Method not allowed.\n`. Console 303s carry the same HTML body, and their `Location` is absolute — resolved from `X-Forwarded-Proto` (or the TLS scheme) and the request `Host`, exactly as Go's `http.Redirect` built it. |
| Method mismatch | A path registered for other methods answers 405 with Go's `ServeMux` shape: `Allow` listing the registered methods sorted alphabetically with `HEAD` included for every `GET` pattern (`Allow: DELETE, GET, HEAD, PATCH` for `/api/links/{slug}`), plus the plain-text body `Method Not Allowed\n`. |
| JSON API | Field names, key order, `X-API-Key` 401 envelope, 415 `Content-Type must be application/json.`, 409 conflict, the validation envelope `{"error":"Validation failed.","fields":{…}}` with Go's messages, 201 on create, 204 on delete and HTMX mutations, 16 KiB body cap reported as `The request body must be valid JSON.` (400, never 413). Timestamps are RFC3339 with Go's trailing-zero trimming (`…T10:00:00.1234567Z`). Bodies end with a newline, and `<`, `>`, `&` inside strings are escaped as `\u003c`, `\u003e`, `\u0026` exactly like `json.Marshal`. |
| Expiration input | `expiresAt` is parsed with Go's `time.Parse(time.RFC3339, …)` grammar: `T` separator, zone `Z` or `±hh:mm` only (the compact `+0700` is rejected), fractional seconds allowed. The console field uses Go's `2006-01-02T15:04` layout, minute precision, read as UTC. |
| List cursor | `{"links":[…],"cursor":{"next":"…"},"total":N}`; the cursor payload is `{"v":1,"sort":…,"dir":…,"n":…,"s":…,"id":…}` with Go's key order and default limit 10, clamped to 1..100. |
| Sessions | Cookie `shorten_session=<base64url(JSON)>.<signature>`, `Path=/; HttpOnly; SameSite=Strict`, `Max-Age` = session TTL, `Expires` in RFC1123 GMT, `Secure` only when the request scheme is HTTPS. |
| Console mutations | `POST` bodies are form-encoded with `csrf`; a missing/invalid token or a foreign `Origin` answers 403 `Invalid security token.`. HTMX mutations answer 204 with `hx-redirect`; non-HTMX answers 303 to the same Location. Notices are `created`, `saved`, `renamed`, `deleted`, `cleaned` (+ `deleted=N` on cleanup). |
| Metrics | `text/plain; version=0.0.4; charset=utf-8`, the fixed line set: `shorten_http_requests_total{route,status}`, `shorten_link_mutations_total{operation}`, `shorten_redirect_requests_total`, `shorten_redirect_duration_seconds_sum`, `shorten_redirects_total{outcome}`, with Go's `# HELP`/`# TYPE` text. |
| Assets | `/assets/{name}` with `cache-control: public, max-age=31536000, immutable`, weak ETag `W/"<16 hex>"`, 304 on `If-None-Match`, HEAD without body, 404 for unknown names. `/assets/styles.css` is the compiled design-system stylesheet. |
| Storage | Byte-identical migration files (`crates/shorten/src/store/migrations`), WAL journal, foreign keys on, `user_version`-free `schema_migrations(version, applied_at)` ledger, i64 UTC-millisecond timestamps, the FTS5 table and its triggers, `VACUUM INTO` backups with the `backup=… manifest=… links=… visits=…` line, and `os.MkdirAll`-shaped failures reported as `mkdir <path>: permission denied`. |
| CLI | Go `flag` grammar (`-flag value`, `-flag=value`, `--flag`, `--` terminator, bare `-` stops parsing, bool never takes the next argument), `Usage of <argv0>:` usage on stderr with `PrintDefaults` layout, exit 0 for `-h`/`-help`, exit 2 for parse failures with Go's messages, exit 1 for runtime failures. Report texts, defaults and number formats match, including `time.ParseDuration` (`1h30m`, `1.5s`, `250ms`, `time: unknown unit " minutes" in duration "5 minutes"`). |
| Graceful shutdown | `ReadHeaderTimeout` 5s, `ReadTimeout` 15s, `WriteTimeout` 30s, `IdleTimeout` 60s, `MaxHeaderBytes` 1 MiB, per-connection `graceful_shutdown` bounded by `SHORTEN_SHUTDOWN_TIMEOUT`, then analytics flush, WAL checkpoint, close. |

## Accepted deviations

These are the places where the Rust stack cannot reproduce Go's bytes. Nothing
behavioural depends on them: status codes, exit codes, headers, counters and
bodies all match; what differs is the wording of an operating-system-level
string, a header name's casing, and a template's escape form.

1. **HTTP transport text in `bench` and `querybench`.** Go prints the Go
   standard library's dialer message, e.g.
   `updated request: Get "http://127.0.0.1:1/…": dial tcp 127.0.0.1:1: connect:
   connection refused`. reqwest phrases the same failure as
   `updated request: error sending request for url (http://127.0.0.1:1/…)`.
   Scenario prefix, line count, stdout emptiness and exit status 1 match.
2. **JSON syntax messages inside `kvimport` rejections.** Go counts malformed
   records and never prints the decoder message; where a message is visible in
   a report, serde_json's text (`key must be a string at line 1 column 2`)
   replaces `encoding/json`'s (`invalid character 'o' in literal null
   (expecting 'u')`). Counters, policy decisions and the summary line match.
3. **`time.ParseDuration` overflow.** Go accumulates duration segments and
   lets the final sum overflow silently at the last addition; the port rejects a
   total above `int64` nanoseconds up front. Both reject `100000000000h` and
   `2562047h47m16.854775808s`.
4. **Header name casing on the wire.** Go writes canonical header names
   (`Allow:`, `Location:`, `Set-Cookie:`); the Rust server writes them
   lower-case (`allow:`, `location:`, `set-cookie:`) because the `http` crate
   normalises header names. HTTP header field names are case-insensitive, and
   every client that reads these responses (browsers, curl, htmx) treats them
   case-insensitively, so no behaviour depends on it.
5. **Secondary-text contrast.** `assets/app.css` is byte-identical to the file the previous
   service shipped and reads `--muted` as a text colour; the compiled design-system stylesheet
   binds `--muted` to that readable Secondary Ink (`#576170`) and gives the recessed wash its own
   token (`--surface-subtle`), so the rendered text is a shade darker than the Go palette's
   `#626a78` in the places the redesign touched. The bytes of `app.css`, the asset headers and the
   markup are unchanged; only the token bindings inside `styles.css` differ, and only in the
   secondary-text greys.
6. **Asset cache-buster token.** The console markup requests `/assets/styles.css?v=20261118-1`
   and `/assets/app.css?v=20261118-1`; the previous service shipped `?v=20260810-2`. The token is
   bumped because the stylesheet bytes changed (secondary-ink binding), and the asset is served
   `max-age=31536000, immutable`, so a fresh token is what makes existing browsers drop the old
   file. Paths, headers and every other byte of the markup are unchanged.
7. **HTML escaping in rendered markup.** Askama escapes interpolated `<` as
   `&#60;` where Go's templ emitted `&lt;`; both decode to the same text in the
   browser. Template tests assert markers and substrings, never byte-equal HTML.

## Data compatibility

A database created by the Go service is opened by the Rust service without
migration: the same `schema_migrations` ledger rows, the same tables, indexes,
FTS5 table and triggers, the same `id`/`created_at`/`updated_at`
millisecond integers, and the same cursor encoding, so an in-flight cursor
issued by one implementation reads correctly from the other. The import path
(`kvimport`) reproduces Go's canonical checksum struct, its `TrimPrefix`,
`encoding/json` binding and `bufio.Scanner` rules, so `kv-export.ndjson`
imports to the same counters, checksum and `verified` flag.

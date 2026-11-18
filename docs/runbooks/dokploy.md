# Deploying on Dokploy

`compose.dokploy.yaml` is the Dokploy flavour of the deployment: the service joins the
external `dokploy-network`, exposes `8080` to the proxy, keeps the application image
read-only and stores the SQLite database on the named volume `shorten_data`. Dokploy
interpolates the Compose file before it injects the service environment, so every
variable Dokploy must supply is written with a default (`${VAR:-…}`) or with an
explicit requirement (`${VAR:?…}`). When a required variable is missing, Compose
aborts with the message baked into the file, e.g.:

```text
error while interpolating services.shorten.environment.SHORTEN_ADMIN_API_KEY:
required variable SHORTEN_ADMIN_API_KEY is missing a value: Configure SHORTEN_ADMIN_API_KEY in Dokploy
```

That is not an application failure; it means the three required variables below have
not been defined in the place Dokploy uses for Compose interpolation.

## The three required variables

| Variable | Shape | Example |
| --- | --- | --- |
| `SHORTEN_PUBLIC_BASE_URL` | `https://` + exact host, no path, no trailing slash | `https://links.example.com` |
| `SHORTEN_ADMIN_API_KEY` | ≥ 16 characters, hex is convenient | `a2d908244ceca4a8952617563b84b9858d040d50e3dcda059181f7e7effd8e7f` |
| `SHORTEN_SESSION_SECRET` | ≥ 32 characters, must differ from the API key | `6079419a437b9bd6264b5f8e6b6ba76899536305b7e6bd943d6498f6a339e636` |

Generate fresh values on the deployment machine — never reuse the strings printed in
this repository:

```sh
openssl rand -hex 32   # SHORTEN_SESSION_SECRET (32 random bytes, printed as 64 hex digits)
openssl rand -hex 32   # SHORTEN_ADMIN_API_KEY
```

`SHORTEN_PUBLIC_BASE_URL` is the exact origin browsers see: the scheme, host and (for a
non-standard port) the port of the public site, e.g. `https://links.example.com`. Short
URLs are generated from it, and browser mutations are validated against it. A value
with a trailing slash, a path, or a mismatched scheme makes short URLs point at the
wrong host and makes the console reject same-site form posts.

## Paste-ready environment block

Set these in the **Environment** area of the Dokploy Compose service (Dokploy writes
them to the `.env` used for Compose interpolation). Everything except the three
required values has a default in the Compose file, so the block can stay short:

```dotenv
# Required — Compose fails without these three
SHORTEN_PUBLIC_BASE_URL=https://links.example.com
SHORTEN_ADMIN_API_KEY=a2d908244ceca4a8952617563b84b9858d040d50e3dcda059181f7e7effd8e7f
SHORTEN_SESSION_SECRET=6079419a437b9bd6264b5f8e6b6ba76899536305b7e6bd943d6498f6a339e636

# Image: use the built-in build (compose.dokploy.yaml has a build section), a registry
# image you push yourself, or the image Dokploy built from the repository.
# SHORTEN_IMAGE=ghcr.io/your-org/shorten:latest

# Only while rotating SHORTEN_SESSION_SECRET: keep the previous secret here for the
# duration of the old sessions (default SHORTEN_SESSION_TTL = 12h), then remove it.
# SHORTEN_PREVIOUS_SESSION_SECRET=

# Optional tuning; the Compose file defaults match these values.
SHORTEN_READ_CONNECTIONS=4
SHORTEN_CACHE_CAPACITY=10000
SHORTEN_CACHE_POSITIVE_TTL=5m
SHORTEN_CACHE_NEGATIVE_TTL=10s
SHORTEN_ANALYTICS_INTERVAL=250ms
SHORTEN_ANALYTICS_THRESHOLD=1000
SHORTEN_ANALYTICS_MAX_KEYS=50000
SHORTEN_SESSION_TTL=12h
SHORTEN_SHUTDOWN_TIMEOUT=15s
```

Values are quoted by Compose the way the file spells them; a value containing spaces is
safe because Dokploy injects them as environment entries, not as shell text. Never put
secrets in the Compose file itself — the file is committed, the Environment area is not.

## Why `SHORTEN_ALLOW_NULL_ORIGIN=true`

`compose.dokploy.yaml` sets `SHORTEN_ALLOW_NULL_ORIGIN: "true"`. Dokploy's generated
domain routes proxy the console through an opaque-origin context, so mutating browser
requests arrive with `Origin: null` instead of the site origin. The application
rejects `null` origins by default (it is the CSRF defence), and this flag lets the
console's own form posts through that proxy. Keep it `false` for direct deployments
where the browser sends the real origin.

## Deploy

1. Point the Dokploy Compose service at the repository, set the Compose file to
   `compose.dokploy.yaml`, and paste the Environment block above.
2. Deploy. Dokploy builds the image (`rust:1-slim` stage runs `cargo test --workspace`
   and `cargo build --release`) and starts the container as uid 10001.
3. Attach the Dokploy domain to the service on port `8080`, enable HTTPS, and make sure
   the proxy forwards `X-Forwarded-Proto: https` so the session cookie gets its
   `Secure` flag.
4. Check it from the host:

```sh
curl -fsS https://links.example.com/healthz   # {"status":"ok"}
curl -fsS https://links.example.com/readyz    # {"status":"ready"}
curl -fsSI https://links.example.com/assets/styles.css | grep -i cache-control
# cache-control: public, max-age=31536000, immutable
```

5. Open `https://links.example.com/manage/login` and sign in with
   `SHORTEN_ADMIN_API_KEY`.

## Rotating the session secret

Set the new `SHORTEN_SESSION_SECRET`, copy the old value into
`SHORTEN_PREVIOUS_SESSION_SECRET`, deploy, and keep both for at least
`SHORTEN_SESSION_TTL` (12 h by default) so existing sessions keep validating. Then
remove `SHORTEN_PREVIOUS_SESSION_SECRET` and deploy again. Rotating the API key needs
no coordination — it is checked directly.

## Database volume and backups

The database lives on `shorten_data` at `/data/shorten.db`. It must be a single-writer
volume: run exactly one replica. Backups use SQLite's online backup inside the
container, so they are safe while the service is running:

```sh
docker compose -f compose.dokploy.yaml exec shorten \
  backup -database /data/shorten.db -output /data/backup-$(date +%Y%m%d).db
```

The command prints `backup=/data/backup-20261118.db manifest=/data/backup-20261118.db.manifest.json
links=… visits=…`. The manifest is a JSON summary of the snapshot, and the snapshot is
a consistent, standalone database file. The service container is `read_only`, so
`/data` is the only writable target for the snapshot.

## Troubleshooting the interpolation errors

| Message | Cause | Fix |
| --- | --- | --- |
| `required variable SHORTEN_PUBLIC_BASE_URL is missing a value` | No `SHORTEN_PUBLIC_BASE_URL` in the Environment area Dokploy interpolates with. | Add it (exact origin, no trailing slash) to the Compose service Environment, then redeploy. |
| `required variable SHORTEN_ADMIN_API_KEY is missing a value` | Same for the admin key. | Add it; at least 16 characters. |
| `required variable SHORTEN_SESSION_SECRET is missing a value` | Same for the signing secret. | Add it; at least 32 characters, different from the API key. |
| Variables present, same errors | Dokploy put them in the domain/global env, not in the Compose service's Environment tab, so Compose never sees them. | Move them to the service's Environment area. |

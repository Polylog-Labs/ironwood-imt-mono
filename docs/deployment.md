# Deployment

[Back to the README](../README.md)

## Run the indexer on a VPS

Keep one Rust process and its SQLite database on persistent local disk. Use remote lightwalletd/RPC providers. Build a Linux release binary for the VPS architecture with `make build` on a machine with Node.js 22.18+, npm, and Rust. Make installs the pinned TypeScript dependencies and Cargo embeds the compiled UI. Copy the release binary to the VPS, or run `make build` directly on the VPS if those tools are installed.

```sh
./ironwood-indexer --public-rpc \
  --database /var/lib/ironwood/ironwood.sqlite \
  --bind 127.0.0.1:3000
```

Create the database directory and give the service user write access. Run under a process supervisor, with an HTTPS reverse proxy forwarding requests to port 3000. The embedded UI serves pool activity at `/` and the explorer at `/ironwood`. You can also host the UI on Vercel.

For a small installation, start with 1 shared vCPU, 1 GiB RAM, and 25 GiB SSD. This is a starting allocation, not a measured capacity guarantee. At the default 10-second refresh, 100 open dashboards generate about 10 dashboard requests/second, plus exploration requests. Each visitor has an independent rate allowance. UI requests read SQLite rather than making upstream RPC calls.

## Deploy the UI on Vercel

Vercel serves the HTML, CSS, and compiled TypeScript and forwards `/v1/*`, `/healthz`, and `/readyz` to your VPS. The sync process and SQLite database remain on the VPS. There are no Vercel Functions or browser runtime npm dependencies.

The Vercel build adds Web Analytics to both `/` and `/ironwood` using its deferred, same-origin tracking script. Enable Web Analytics for the project before deploying; view traffic in the project's Analytics tab. Analytics requests stay on Vercel and do not hit the Rust API. Local and embedded Rust pages do not include tracking.

1. Import the repository into Vercel. Set **Root Directory** to the repository root and **Framework Preset** to **Other**.
2. Leave the build settings supplied by `vercel.json` in place: **Build Command** is `npm run build:vercel`; **Install Command** is `npm ci --ignore-scripts`. Leave **Output Directory** unset. Use Node.js 22.18 or newer. The build checks all TypeScript and compiles the browser modules. You can deploy now; the UI shows a standby view until the API is available.
3. Make the VPS API reachable at a public HTTPS origin, for example `https://api.example.com`. Check that `/healthz` returns JSON and `/v1/dashboard` returns data. `/readyz` returns 503 until chain and mempool observations are current.
4. Add **`IRONWOOD_API_ORIGIN=https://api.example.com`** to the Production and Preview environments you intend to deploy, then redeploy. Use the API origin only, without `/v1`, credentials, query parameters, or a fragment. It must be reachable from Vercel; localhost points to the build/deployment environment, not your VPS.

The build emits `.vercel/output/` using Vercel's [Build Output API](https://vercel.com/docs/build-output-api). With no API origin it serves the UI and returns an explicit 503 JSON response for API requests; it does not present fabricated chain data. An invalid configured origin fails the build. Changing the origin requires a new build/deployment. The origin is configuration, not a place for RPC credentials.

Browser requests stay on the UI's origin through Vercel's [external rewrites](https://vercel.com/docs/routing/rewrites), so no browser CORS configuration is needed. API responses bypass caching to preserve freshness and reorg handling. Static assets revalidate because their filenames are not versioned. No database, screenshot, or Rust build artifacts enter the static output.

While the API is unavailable, the explorer shows neutral metric placeholders and a standby panel with automatic and manual retry. Tree controls appear after the first verified snapshot arrives. If the API goes offline later, the last loaded tree and camera remain visible with a UTC snapshot timestamp.

The Rust API includes a 20-request/second limiter with a 240-request burst per client IP. Configure proxy identity as described below and deploy the updated Rust binary to enable it. Publishing the UI to Vercel does not update the API process.

Check the packaging locally, without deploying:

```sh
npm ci --ignore-scripts
npm run build:vercel # UI only, when IRONWOOD_API_ORIGIN is unset
IRONWOOD_API_ORIGIN=https://api.example.com npm run build:vercel
npm test
```

This verifies the artifact and configuration; it does not check connectivity to the configured origin. After deployment, check `/v1/dashboard` through the Vercel URL, then open `/ironwood` and select **Latest leaf** to verify snapshot queries and leaf inspection.

## Per-client rate limiting

For **Vercel → Nginx → Rust**, generate a random secret with `openssl rand -hex 32`. Set the same `IRONWOOD_PROXY_TOKEN` in the Rust service environment and in Vercel's Production environment (and Preview if needed), then restart the backend and redeploy the UI. The build puts it in a server-side **request** header transform for API rewrites only; it is never included in browser assets or response headers. Vercel overwrites `X-Vercel-Forwarded-For` with the visitor's IP, and Rust trusts it only when the token matches. Keep Nginx's default request-header forwarding enabled and do not log the token header. This requires no Vercel Functions.

For **direct clients → Nginx → Rust**, set `IRONWOOD_TRUSTED_PROXIES=127.0.0.1,::1` on Rust and add the following to Nginx's API proxy location:

```nginx
proxy_set_header X-Real-IP $remote_addr;
```

Trust only exact proxy connection IPs you control and ensure they **overwrite** this header. Configure both mechanisms when the API accepts direct callers and Vercel traffic. Authenticated Vercel identity takes precedence; direct callers use the IP set by Nginx. Do not trust arbitrary `X-Forwarded-For` or enable proxy trust without configuring the proxy. Without proxy configuration Rust uses the connection IP, which combines visitors behind that proxy. Users sharing a public IP share a bucket; this anonymous API has no account identity.

References: [Vercel request headers](https://vercel.com/docs/headers/request-headers), [request transforms](https://vercel.com/docs/build-output-api/configuration#source-route-transforms).

## Storage and operations

Storage grows with occupied actions. A fully allocated array of 2³² 32-byte leaves alone would need 128 GiB; this indexer stores populated leaves and completed subtrees and derives empty nodes. Historical views share immutable subtree data rather than copying a tree per block.

Allow space for the database, WAL, and backups. Track disk growth and upstream provider limits. Increase `--poll-seconds` to reduce RPC traffic, or decrease `--batch-size` if a range exceeds provider deadlines or memory limits. Public providers do not guarantee throughput or availability.

Use SQLite's online backup mechanism while the service is running:

```sh
sqlite3 /var/lib/ironwood/ironwood.sqlite '.backup /path/to/backup.sqlite'
```

Do not copy just the database file while WAL writes are active. Keep one indexer per database. A separate database server is unnecessary until measured concurrency or multi-writer requirements justify it.

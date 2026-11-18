# Deployment

[Back to the README](../README.md)

## Run the indexer on a VPS

Keep one Rust process and its SQLite database on persistent local disk. Use remote lightwalletd/RPC providers, so the VPS does not need a full Zcash node. Build a Linux release binary for the VPS architecture on a build machine or in CI.

```sh
./ironwood-indexer --public-rpc \
  --database /var/lib/ironwood/ironwood.sqlite \
  --bind 127.0.0.1:3000
```

Create the database directory and give the service user write access. Run under a process supervisor, with an HTTPS reverse proxy forwarding requests to port 3000. Serve the embedded UI from `/`, or use Vercel for the UI below.

For a small installation, start with 1 shared vCPU, 1 GiB RAM, and 25 GiB SSD. This is a starting allocation, not a measured capacity guarantee. At the default 10-second refresh, 25 open dashboards generate about 2.5 dashboard requests/second, plus exploration requests. UI requests read SQLite rather than making upstream RPC calls.

## Deploy the UI on Vercel

Vercel serves the HTML, CSS, and JavaScript and forwards `/v1/*`, `/healthz`, and `/readyz` to your VPS. The sync process and SQLite database remain on the VPS. There are no Vercel Functions or frontend package dependencies.

1. Make the VPS API reachable at a public HTTPS origin, for example `https://api.example.com`. Check that `/healthz` returns JSON and `/v1/dashboard` returns data. `/readyz` returns 503 until chain and mempool observations are current.
2. Import the repository into Vercel. Set **Root Directory** to the repository root and **Framework Preset** to **Other**.
3. Add **`IRONWOOD_API_ORIGIN=https://api.example.com`** to the Production and Preview environments you intend to deploy. Use the API origin only, without `/v1`, credentials, query parameters, or a fragment. It must be reachable from Vercel; localhost points to the build/deployment environment, not your VPS.
4. Leave the build settings supplied by `vercel.json` in place: **Build Command** is `node scripts/build-vercel.mjs`; **Install Command** is empty. Leave **Output Directory** unset. Use Node.js 22 or newer. Deploy when ready.

The build emits `.vercel/output/` using Vercel's [Build Output API](https://vercel.com/docs/build-output-api). It fails with a clear error if the API origin is missing or invalid. Changing the origin requires a new build/deployment. The origin is configuration, not a place for RPC credentials.

Browser requests stay on the UI's origin through Vercel's [external rewrites](https://vercel.com/docs/routing/rewrites), so no browser CORS configuration is needed. API responses bypass caching to preserve freshness and reorg handling. Static assets revalidate because their filenames are not versioned. No database, screenshot, or Rust build artifacts enter the static output.

Check the packaging locally, without deploying:

```sh
IRONWOOD_API_ORIGIN=https://api.example.com node scripts/build-vercel.mjs
node --test tests/*.mjs
```

This verifies the artifact and configuration; it does not check connectivity to the configured origin. After deployment, check `/v1/dashboard` through the Vercel URL, then open the explorer and select **Latest leaf** to verify snapshot queries and leaf inspection.

## Storage and operations

Storage grows with occupied actions. A fully allocated array of 2³² 32-byte leaves alone would need 128 GiB; this indexer stores populated leaves and completed subtrees and derives empty nodes. Historical views share immutable subtree data rather than copying a tree per block.

Allow space for the database, WAL, and backups. Track disk growth and upstream provider limits. Increase `--poll-seconds` to reduce RPC traffic, or decrease `--batch-size` if a range exceeds provider deadlines or memory limits. Public providers do not guarantee throughput or availability.

Use SQLite's online backup mechanism while the service is running:

```sh
sqlite3 /var/lib/ironwood/ironwood.sqlite '.backup /path/to/backup.sqlite'
```

Do not copy just the database file while WAL writes are active. Keep one indexer per database. A separate database server is unnecessary until measured concurrency or multi-writer requirements justify it.

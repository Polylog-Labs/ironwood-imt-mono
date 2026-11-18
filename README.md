# Ironwood indexer

A single Rust crate and service that indexes all public Ironwood note commitments and nullifiers from a Zcash RPC, verifies the commitment tree, persists block deltas in SQLite, and serves a read-only HTTP API with an interactive local tree explorer.

**The depth-32 tree is the note commitment tree.** Its capacity is 2³² leaves; it is not necessarily full. Zcash maintains a separate Ironwood nullifier set. This service indexes both, but does not invent a consensus nullifier-tree root or implement the separate voting/PIR indexed nullifier tree. Note commitments and nullifiers cannot be used to link a spent note to its original commitment.

## Run

Use a free public **lightwalletd** endpoint. No full node or API key is required. TLS certificate verification is enabled. The Rust toolchain is pinned; SQLite and the protobuf compiler are bundled.

```sh
cargo build --release --locked
./target/release/ironwood-indexer --public-rpc
```

Open `http://127.0.0.1:3000/` for the dashboard. It shows capacity usage, rolling 24-hour growth, pending commitments, the newest leaves, and the verified root. Static HTML/CSS/JS is embedded in the Rust binary; no frontend build or Node runtime is required. It refreshes every 10 seconds.

This starts the HTTP API while history downloads. The public preset uses Stardust, Cake Wallet, Zec.rocks, and ZcashESP, in that order. It is mainnet-only. Stop with Ctrl-C and run the same command to resume from SQLite.

Each chain attempt stays on one provider through reconciliation and verified batches. On a failed attempt, the next eligible provider resumes from the last durable checkpoint. Healthy providers remain selected; failed providers cool down for 10 seconds, doubling up to five minutes. Each operation tries each eligible source at most once. Chain and mempool selection/cooldowns are independent. Genesis and network must agree. With multiple sources, a provider behind the stored height is rejected instead of causing a rollback; a real rollback to a shorter chain requires explicit single-provider operation or waiting for the network to regain that height.

To choose your own ordered providers, repeat `--lightwalletd-url` or use comma-separated URLs:

```sh
./target/release/ironwood-indexer \
  --lightwalletd-url https://eu.zec.stardust.rest:443 \
  --lightwalletd-url https://zec-node.cakewallet.com:443
```

One URL still works. The preset and explicit gRPC URLs cannot be combined with JSON-RPC URL/authentication flags. `--rpc-url` continues to support any Zebra-compatible JSON-RPC provider implementing the contract below; method support is checked, not assumed from a provider's brand. Public services offer no throughput or availability guarantee.

For a private Ironwood-capable **Zebra** JSON-RPC node instead:

```sh
export ZCASH_RPC_URL=http://127.0.0.1:8232
export ZCASH_RPC_COOKIE=/path/to/node/.cookie
./target/release/ironwood-indexer
```

Alternatively set `ZCASH_RPC_USER` and `ZCASH_RPC_PASSWORD`. Omit authentication if the node does not require it. Do not put credentials in URLs. Cookies are reread to support rotation. Environment files are not loaded automatically. `ZCASH_LIGHTWALLETD_URL` accepts a comma-separated list, equivalent to repeating `--lightwalletd-url`. JSON-RPC authentication is never sent to the public gRPC providers.

Defaults: database `data/ironwood.sqlite`, API `127.0.0.1:3000`, 500 blocks per gRPC batch (`--batch-size`), 10-second polling, and a 30-second request/batch deadline. JSON-RPC uses four concurrent block and mempool fetches. Run `--help` for configuration. `RUST_LOG=ironwood_indexer=debug` controls logging.

```sh
# Catch up once, print the checkpoint and mempool sample, then exit.
./target/release/ironwood-indexer --public-rpc --once

curl http://127.0.0.1:3000/v1/status
curl http://127.0.0.1:3000/v1/providers
curl 'http://127.0.0.1:3000/v1/leaves?from=0&limit=100'
curl http://127.0.0.1:3000/v1/tree/nodes/32/0
curl http://127.0.0.1:3000/v1/mempool
```

NU6.3 activation is discovered through JSON-RPC. lightwalletd only advertises the next upgrade, so its adapter uses the published mainnet (3,428,143) and testnet (4,134,000) activation heights. `--start-height` must match the known activation, or supply it for custom networks. The preceding tree must be empty: starting at chain tip would omit historical leaves. The service fails clearly if the node has not reached activation, uses the wrong network, or lacks required Ironwood fields. Startup tries each eligible provider before exiting if none can initialize; a process supervisor can restart it. Runtime failures keep the last verified checkpoint and retry with exponential backoff, from the polling interval up to five minutes (or the configured interval, if longer).

## Data and correctness

1. Check network, genesis, activation, and the persisted checkpoint.
2. Find the common ancestor if the node's canonical chain changed; remove orphaned blocks, leaves, and subtrees atomically.
3. Fetch blocks with bounded concurrency and apply them in transaction/action order.
4. Extract **only** `ironwood.actions[].cmx` and `.nullifier`, including dummy actions. Decode canonical field encodings. Verify cumulative leaf count and compute the root with the upstream Orchard Sinsemilla implementation.
5. Verify tree roots against the source. JSON-RPC uses `z_gettreestate` for each block hash. gRPC streams a bounded `GetBlockRange`, checks parent hashes, original transaction indexes, and cumulative counts, and reconstructs roots from the starting frontier. The ending root/count/hash must match `GetTreeState` before any block in that batch is persisted. Each block is then checked against the independent SQLite tree implementation. The complete gRPC batch commits atomically with one disk flush, retaining the checkpoint/root for every block.
6. Poll the mempool independently. gRPC uses `GetMempoolTx` with Ironwood filtering and publishes only complete snapshots. JSON-RPC fetches only new transaction IDs and evicts IDs no longer present, using a separate cache per provider. A sample always comes from one source; independent mempools are never merged. Pending commitments never receive confirmed positions or modify the tree.

There is one durable SQLite writer and one reader on separate worker threads. WAL permits API reads during writes. `synchronous=FULL` and atomic writes preserve checkpoints across interruptions. JSON-RPC commits per block; gRPC commits a verified batch. Restart downloads only the suffix after the last durable checkpoint, including any interrupted batch. Roots and leaf counts are recorded for every indexed block, including empty blocks. Chain-derived data is retained; mempool samples are intentionally ephemeral and rebuilt after restart.

This is an indexer trusting a validating node, **not an independent consensus validator**. Root comparison detects parsing/order/storage errors; it does not authenticate the RPC itself. Nullifiers are protected by canonical encoding and database uniqueness, but their membership is trusted to the node. Transactions can leave the mempool during sampling: `disappeared_during_sample` reports this. Mempool freshness describes an observation, not an atomic network-wide snapshot. A transient reorg may leave the previously observed branch visible until the next successful reconciliation.

## HTTP API

All routes are GET-only. `/v1/status` also includes the provider status. Provider labels expose URL origins only, excluding JSON-RPC paths and query strings that might contain API keys. Provider `last_success` records the last completed operation (initialization, catch-up pass, or mempool sample); it does not track individual downloaded batches. Hash values are 64-character hex strings. Block/transaction IDs retain RPC display order. Commitments, nullifiers, and tree hashes use canonical little-endian field bytes; Zebra's `finalRoot` display encoding is reversed before comparison.

| Route | Result |
| --- | --- |
| `/` | Interactive tree explorer |
| `/v1/dashboard` | One database snapshot of occupancy, growth, root, and the 12 newest leaves, with sync/mempool freshness |
| `/healthz` | Process liveness |
| `/readyz` | 200 when caught up to a recent sampled node tip and mempool sample; otherwise 503 |
| `/v1/status` | Checkpoint, tree capacity, sync progress, freshness, reorg generation |
| `/v1/providers` | Selected sources, transport, consecutive failures, cooldown, and last completed operation, separately for chain and mempool |
| `/v1/leaves?from=0&limit=100&at=HASH` | Commitment and nullifier rows in confirmed leaf order, with containing block timestamps (`time`) |
| `/v1/nullifiers?from=0&limit=100&at=HASH` | The same action rows, including all nullifiers |
| `/v1/nullifiers/HEX` | Current confirmed membership and source action, or `found: false` |
| `/v1/blocks?from=HEIGHT&limit=100&at=HASH` | Block roots, leaf counts, and added position ranges |
| `/v1/tree/nodes/LEVEL/INDEX?at=HASH` | Node hash, occupied count and position interval, including empty nodes |
| `/v1/mempool?from=0&limit=100&sequence=N` | Pending Ironwood commitments/nullifiers and observation time |

Rolling growth is `(current_count - baseline_count) / baseline_count × 100`, where the baseline is the highest canonical block at or before server time minus 86,400 seconds, using block timestamps. The growth endpoint includes the actual baseline checkpoint. Missing history, a zero denominator, or a checkpoint older than the window produces no percentage; the dashboard also withholds it while chain state is stale. A brief gap to a newly sampled tip does not hide a recent verified reading.

`from` is inclusive; `next` is the next `from`, or null. Limits are 1–1000. Tree level 0 is a leaf, level 32 is the root; node indices start at zero. Position/count types are 64-bit so a full tree's **count** of 2³² fits.

For a consistent export, take `snapshot.checkpoint.hash` from the first page and pass it as `at` on every subsequent request. A surviving historical snapshot remains stable as the tip grows. An orphaned snapshot returns **409**, requiring a restart. The mempool's `sequence` provides the same protection within one process; refreshes advance it. Neither leaves nor tree nodes are read from the RPC on API requests.

For animation, poll status, then fetch new `/v1/blocks` rows and their position ranges from `/v1/leaves`. On a `generation` change, discard cached branch-dependent state and reload. Canonical historical roots/nodes remain queryable; orphan history and old mempool samples are not archived.

## Tree explorer

Open the API origin in a browser (locally, `http://localhost:3001/` when started with `--bind 127.0.0.1:3001`). The Rust binary embeds all assets; no frontend server, JavaScript runtime, external fonts, or CDN is needed in production.

Drag or scroll horizontally to pan, scroll vertically or pinch to zoom, and click a node to inspect it. Shift-scroll also pans horizontally. Double-click a branch to descend. The controls jump to the whole tree, occupied region, latest leaf, or any index including an empty position. Keyboard controls: `+`/`-` zoom, arrow keys pan, `Home` shows the whole tree. Recent leaves and index search also expose individual commitments without pointer navigation.

The canvas renders only the visible interval and a few levels at a time. Coarse nodes represent exact subtree ranges; they are not individual leaves or sampled commitments. Geometry is derived from the depth and verified append count, with no full-tree allocation. Visible leaf reads are capped at 128 per request, the leaf cache at 256 entries, and the pixel ratio at 2. Rendering stops when the camera is idle. API reads use the displayed block hash, cancel obsolete requests, and refresh after a 409 from a reorganization. Polling every 10 seconds preserves the camera and selection.

The inspector shows real subtree hashes and, for occupied leaves, the containing block's timestamp in UTC. This is not a precise arrival time. Empty leaf values are **2**, encoded as `02` followed by 31 zero bytes; empty internal nodes use the recursively hashed empty subtree. UI depth counts down from the root (0–32), while the API's level counts up from leaves (0–32).

The estimated fill date is a **linear** projection: `observed_at + (capacity - current_count) / additions_24h * 86400`. It assumes constant absolute additions per day, not compound percentage growth. Missing/zero growth or stale chain data gives no estimate. An empty leaf at index `i` shows its own likely fill date, using `(i + 1 - current_count) / additions_24h` days; the first empty leaf still requires one addition. Neither mempool values nor empty leaves count toward occupancy or the projection.

## Cost and deployment

Use **SQLite now**, on the same VPS disk as the indexer. There is one ingestion writer, paginated indexed reads, and no need for a database server, queue, cache server, or distributed infrastructure. For about 25 simultaneous viewers, start with 1 shared vCPU, 1 GiB RAM, and 25 GiB SSD: the DigitalOcean Basic Regular plan is [$6/month](https://www.digitalocean.com/pricing/droplets) as of 2026-11-18. This is a starting recommendation, not a measured Droplet capacity guarantee. Run the indexer against remote lightwalletd/RPC providers; a full Zcash node has substantially larger requirements. Build the Linux release binary in CI or on a build machine, then run it under a process supervisor behind an HTTPS reverse proxy on the VPS.

Storage grows with occupied actions, not tree capacity. A fully allocated array of 2³² 32-byte leaves alone is **128 GiB**. Here, each action stores its commitment, nullifier, source transaction, and indices. Completed internal subtrees total fewer than one per occupied leaf; empty subtrees are constants, and partially occupied subtrees are reconstructed in O(32) work. All canonical historical tree views share these immutable nodes; there is no full-tree copy per block.

A short local loopback check on 2026-11-18 completed 300 mixed dashboard, leaf-page, and partial-subtree requests from 25 concurrent clients with no errors and endpoint p95 latencies below 26 ms. The running indexer used about 18 MiB resident memory after that check; this is not a peak-memory measurement. The current database was about 331 MiB plus WAL. This does not measure a shared DigitalOcean CPU or internet latency. With each viewer polling every 10 seconds, 25 open pages produce about 2.5 dashboard requests/second, plus exploration requests.

Plan disk from measured bytes per action × expected actions, plus block metadata, WAL headroom, and backups. The local benchmark below prints bytes/action and ingestion rate; its synthetic workload excludes RPC and should not be treated as a production throughput claim. Do not provision for a hypothetical full tree before measuring actual occupancy.

On 2026-11-18, the release benchmark on an Apple M2 Pro indexed 10,000 synthetic actions in 100 blocks in 1.18 seconds (about 8,500 actions/second), with a 2,330,624-byte database (233 bytes/action). This excludes RPC, fixture generation, WAL, and backups. Random real-world keys, block sizes, disk latency, API load, and VPS hardware will change these results.

The gRPC backfill uses three calls per batch: the starting frontier, a streamed compact block range, and the ending frontier. At the default 500-block batch size, 158,000 blocks require roughly 948 backfill calls, plus initialization, retries, and mempool sampling. With no new blocks, a gRPC chain pass costs four reads and a mempool sample one. A 10-second poll interval therefore has a base cost of about 43,200 calls/day.

JSON-RPC backfill uses two calls per block: verbose `getblock` and hash-pinned `z_gettreestate`. A no-change chain pass costs three reads and a stable mempool one (about 34,560 base calls/day at a 10-second interval). New mempool IDs add one transaction read each. Increase `--poll-seconds` for metered providers. Identity validation adds a few calls on first use of each provider. API traffic never creates node RPC traffic.

Use one process per database on a local filesystem. The reader has an 8-MiB SQLite cache; the writer has 16 MiB. JSON-RPC bodies and complete gRPC batches are capped at 64 MiB, individual gRPC messages at 16 MiB, concurrency is bounded, and oversized mempools fail without presenting a truncated sample as complete (50,000 transactions / 250,000 Ironwood actions). gRPC runs one sequential history stream and one independent mempool stream. JSON-RPC allows up to eight simultaneous requests across the two workers. Reduce `--batch-size` if a range exceeds the size/deadline limit. CPU and memory limits should account for JSON buffers, hashing, and cached mempool actions.

Serve the eventual website and `/api` through one reverse proxy with TLS, caching, request limits, and rate limits; proxy `/api/*` to the loopback API with the prefix removed. No CORS policy or public authentication scheme is imposed yet. Keep RPC credentials and the node RPC private. The API continues serving verified historical data during catch-up; consumers should inspect readiness/freshness before calling it current.

Back up using SQLite's online backup API or `sqlite3 data/ironwood.sqlite '.backup backup.sqlite'`, not a plain copy of a live WAL database. Backups reduce recovery time; the chain database is reconstructible from the node. Switch to Postgres only if independently measured concurrency or multi-writer requirements justify the operational cost.

## Validate

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
node --test tests/tree-model.mjs # optional Node 18+ for frontend geometry/projection tests
cargo run --release --locked --example bench_ingest -- 10000
# Optional network check: all four providers, first ten Ironwood blocks and mempool.
cargo run --release --locked --example check_providers
```

Tests use a real HTTP mock RPC, temporary SQLite databases, upstream published Merkle vectors, and the upstream frontier as an independent tree-storage reference. They cover resume, reorgs, atomic failures, historical queries, pagination, mempool churn, authentication rotation, and unsupported responses. A captured mainnet compact-block fixture checks the first 97 Ironwood actions, original transaction positions, root reconstruction, resume, malformed frontiers, and rejected corrupt ranges. Provider tests cover failover after partial commits, sticky selection, lag protection, network/genesis rejection, separate mempool caches, cooldowns, and API URL redaction. Live sync still depends on the selected server supporting the pinned protocol.

## RPC contract and sources

JSON-RPC methods: `getblockchaininfo`, `getblockhash`, `getblock` (verbosity 1 and 2, height string), `z_gettreestate` (block hash), `getrawmempool(false)`, and `getrawtransaction(txid, 1)`. Historical transaction indexing is unnecessary because confirmed transactions arrive inside verbose blocks. Current mempool transactions must remain available to `getrawtransaction`.

The lightwalletd adapter uses `GetLightdInfo`, `GetLatestBlock`, `GetBlock` (genesis identification), `GetBlockRange`, `GetTreeState`, and `GetMempoolTx`. It requires lightwallet protocol 0.6 or newer for Ironwood pool filtering. Vendored protobuf definitions and their MIT license are in `proto/`, pinned to [lightwallet-protocol ec53075](https://github.com/zcash/lightwallet-protocol/tree/ec53075c706e78a4e107e7d3c92eea4e70262647). Compact transactions do not expose transaction versions; the adapter validates the known Ironwood action fields and does not infer full transaction validity.

- [ZIP 229: separate Ironwood state and v6 transaction format](https://zips.z.cash/zip-0229).
- [Ironwood design](https://zcash.github.io/ironwood/design.html).
- [Zebra RPC block/tree implementation](https://github.com/ZcashFoundation/zebra/blob/720141ff8b705bab4e57d88884f028276e4119d3/zebra-rpc/src/methods.rs): `trees.ironwood.size` and `ironwood.commitments.finalRoot`.
- [Zebra transaction RPC encoding](https://github.com/ZcashFoundation/zebra/blob/720141ff8b705bab4e57d88884f028276e4119d3/zebra-rpc/src/methods/types/transaction.rs): optional `ironwood` bundle with required `actions`, `cmx`, and `nullifier`.
- [Orchard tree implementation](https://github.com/zcash/orchard/blob/0.16.0/src/tree.rs) and [published test vectors](https://github.com/zcash/zcash-test-vectors).

RPC source shapes were inspected on 2026-11-18. Future schema/transaction versions fail closed where required fields or supported versions differ. The JSON-RPC adapter requires Ironwood `finalRoot`; the gRPC adapter verifies against the serialized Ironwood frontier instead.

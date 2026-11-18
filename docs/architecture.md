# Architecture and API reference

[Back to the README](../README.md)

## Provider selection

Each chain attempt stays on one provider through reconciliation and verified batches. On a failed attempt, the next eligible provider resumes from the last durable checkpoint. Healthy providers remain selected; failed providers cool down for 10 seconds, doubling up to five minutes. Each operation tries each eligible source at most once. Chain and mempool selection/cooldowns are independent. Genesis and network must agree. With multiple sources, a provider behind the stored height is rejected instead of causing a rollback; a real rollback to a shorter chain requires explicit single-provider operation or waiting for the network to regain that height.

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

The Rust server limits API requests to **20 per second per client IP**, with a **240-request burst for each IP**. There is no shared request budget across visitors. Excess requests receive **429**, a JSON error, and `Retry-After` in seconds. UI assets are exempt. Buckets live in memory, expire after 60 idle seconds, and are capped at 4,096 clients; a full table temporarily rejects new clients. Limits reset when the process restarts. People sharing a public IP also share its allowance.

Direct connections use the socket peer. Explicitly trusted proxies may supply an overwritten `X-Real-IP`; Vercel may supply `X-Vercel-Forwarded-For` only with the configured shared proxy token. Unauthenticated forwarding headers are ignored. Missing, duplicate or malformed authenticated client IPs fail with 400 rather than bypassing the limiter. See [proxy configuration](deployment.md#per-client-rate-limiting) before deploying behind a proxy; without it the socket peer is the proxy itself.

All routes are GET-only. `/v1/status` also includes the provider status. Provider labels expose URL origins only, excluding JSON-RPC paths and query strings that might contain API keys. Provider `last_success` records the last completed operation (initialization, catch-up pass, or mempool sample); it does not track individual downloaded batches. Hash values are 64-character hex strings. Block/transaction IDs retain RPC display order. Commitments, nullifiers, and tree hashes use canonical little-endian field bytes; Zebra's `finalRoot` display encoding is reversed before comparison.

| Route | Result |
| --- | --- |
| `/` | Coming-soon page with a link to Ironwood |
| `/ironwood` | Interactive tree explorer |
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

The sync bar measures indexed blocks from Ironwood activation to the latest reported upstream tip. Unknown tips show an indeterminate bar; unavailable APIs retain the last reported progress without inventing a percentage. Dashboard polling pauses in hidden tabs and resumes when visible. Rate-limited dashboard requests respect `Retry-After` before retrying.

Drag or scroll horizontally to pan, scroll vertically or pinch to zoom, and click a node to inspect it. Shift-scroll also pans horizontally. Double-click a branch to descend. The controls jump to the whole tree, occupied region, latest leaf, or any index including an empty position. Keyboard controls: `+`/`-` zoom, arrow keys pan, `Home` shows the whole tree. Recent leaves and index search also expose individual commitments without pointer navigation.

The canvas renders only the visible interval and a few levels at a time. Coarse nodes represent exact subtree ranges; they are not individual leaves or sampled commitments. Geometry is derived from the depth and verified append count, with no full-tree allocation. Visible leaf reads are capped at 128 per request, the leaf cache at 256 entries, and the pixel ratio at 2. Rendering stops when the camera is idle. API reads use the displayed block hash, cancel obsolete requests, and refresh after a 409 from a reorganization. Polling every 10 seconds preserves the camera and selection.

The inspector shows real subtree hashes and, for occupied leaves, the containing block's timestamp in UTC. This is not a precise arrival time. Empty leaf values are **2**, encoded as `02` followed by 31 zero bytes; empty internal nodes use the recursively hashed empty subtree. UI depth counts down from the root (0–32), while the API's level counts up from leaves (0–32).

The estimated fill date is a **linear** projection: `observed_at + (capacity - current_count) / additions_24h * 86400`. It assumes constant absolute additions per day, not compound percentage growth. Missing/zero growth or stale chain data gives no estimate. An empty leaf at index `i` shows its own likely fill date, using `(i + 1 - current_count) / additions_24h` days; the first empty leaf still requires one addition. Neither mempool values nor empty leaves count toward occupancy or the projection.

## RPC contract and sources

JSON-RPC methods: `getblockchaininfo`, `getblockhash`, `getblock` (verbosity 1 and 2, height string), `z_gettreestate` (block hash), `getrawmempool(false)`, and `getrawtransaction(txid, 1)`. Historical transaction indexing is unnecessary because confirmed transactions arrive inside verbose blocks. Current mempool transactions must remain available to `getrawtransaction`.

The lightwalletd adapter uses `GetLightdInfo`, `GetLatestBlock`, `GetBlock` (genesis identification), `GetBlockRange`, `GetTreeState`, and `GetMempoolTx`. It requires lightwallet protocol 0.6 or newer for Ironwood pool filtering. Vendored protobuf definitions and their MIT license are in `proto/`, pinned to [lightwallet-protocol ec53075](https://github.com/zcash/lightwallet-protocol/tree/ec53075c706e78a4e107e7d3c92eea4e70262647). Compact transactions do not expose transaction versions; the adapter validates the known Ironwood action fields and does not infer full transaction validity.

- [ZIP 229: separate Ironwood state and v6 transaction format](https://zips.z.cash/zip-0229).
- [Ironwood design](https://zcash.github.io/ironwood/design.html).
- [Zebra RPC block/tree implementation](https://github.com/ZcashFoundation/zebra/blob/720141ff8b705bab4e57d88884f028276e4119d3/zebra-rpc/src/methods.rs): `trees.ironwood.size` and `ironwood.commitments.finalRoot`.
- [Zebra transaction RPC encoding](https://github.com/ZcashFoundation/zebra/blob/720141ff8b705bab4e57d88884f028276e4119d3/zebra-rpc/src/methods/types/transaction.rs): optional `ironwood` bundle with required `actions`, `cmx`, and `nullifier`.
- [Orchard tree implementation](https://github.com/zcash/orchard/blob/0.16.0/src/tree.rs) and [published test vectors](https://github.com/zcash/zcash-test-vectors).

RPC source shapes were inspected on 2026-11-18. Future schema/transaction versions fail closed where required fields or supported versions differ. The JSON-RPC adapter requires Ironwood `finalRoot`; the gRPC adapter verifies against the serialized Ironwood frontier instead.

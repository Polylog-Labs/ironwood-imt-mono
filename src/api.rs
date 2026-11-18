use crate::{
    mempool::SharedMempool,
    model::{Bytes32, unix_time},
    providers::Providers,
    store::{SnapshotGone, Store},
    sync::SharedSyncStatus,
    tree,
};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub mempool: SharedMempool,
    pub sync: SharedSyncStatus,
    pub providers: Option<(Providers, Providers)>,
    pub stale_after_seconds: u64,
}
struct ApiError(StatusCode, &'static str);
impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        if error.is::<SnapshotGone>() {
            Self(
                StatusCode::CONFLICT,
                "snapshot no longer canonical; restart pagination",
            )
        } else {
            tracing::error!(error = %format!("{error:#}"), "API database read failed");
            Self(StatusCode::INTERNAL_SERVER_ERROR, "database read failed")
        }
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}
pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(crate::ui::routes())
        .route("/v1/dashboard", get(dashboard))
        .route("/healthz", get(|| async { Json(json!({"status":"ok"})) }))
        .route("/readyz", get(ready))
        .route("/v1/status", get(status))
        .route("/v1/providers", get(providers))
        .route("/v1/leaves", get(leaves))
        .route("/v1/nullifiers", get(leaves))
        .route("/v1/nullifiers/{nullifier}", get(nullifier))
        .route("/v1/blocks", get(blocks))
        .route("/v1/tree/nodes/{level}/{index}", get(node))
        .route("/v1/mempool", get(mempool))
        .with_state(state)
}
fn fresh(time: Option<u64>, max_age: u64) -> bool {
    time.is_some_and(|t| unix_time().saturating_sub(t) <= max_age)
}
async fn status(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let snapshot = state.store.snapshot().await?;
    let sync = state.sync.read().await.clone();
    let mempool = state.mempool.read().await;
    let chain_fresh = !sync.failed && fresh(sync.last_success, state.stale_after_seconds);
    let caught_up = sync.node_hash == Some(snapshot.checkpoint.hash);
    let mempool_fresh = !mempool.failed && fresh(mempool.observed_at, state.stale_after_seconds);
    Ok(Json(json!({
        "providers": provider_status(&state),
        "snapshot": snapshot, "sync":sync, "ready": chain_fresh && caught_up && mempool_fresh,
        "caught_up_to_sampled_tip": caught_up, "chain_fresh": chain_fresh,
        "tree": {"kind":"ironwood_note_commitment", "depth":tree::DEPTH, "capacity":tree::CAPACITY, "root_encoding":"canonical_little_endian", "verified_against_rpc":true},
        "mempool": {"sequence":mempool.sequence, "observed_at":mempool.observed_at, "actions":mempool.actions.len(), "fresh":mempool_fresh, "failed":mempool.failed}
    })))
}
async fn dashboard(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let now = unix_time();
    let data = state.store.dashboard(now, 12).await?;
    let sync = state.sync.read().await.clone();
    let mempool = state.mempool.read().await;
    let chain_fresh = !sync.failed && fresh(sync.last_success, state.stale_after_seconds);
    let caught_up = sync.node_hash == Some(data.snapshot.checkpoint.hash);
    let mempool_fresh = !mempool.failed && fresh(mempool.observed_at, state.stale_after_seconds);
    Ok(Json(json!({
        "observed_at": now,
        "tree_kind": "ironwood_note_commitment",
        "data": data,
        "sync": sync,
        "chain_fresh": chain_fresh,
        "caught_up": caught_up,
        "ready": chain_fresh && caught_up && mempool_fresh,
        "mempool": {
            "actions": mempool.observed_at.map(|_| mempool.actions.len()),
            "transactions": mempool.observed_at.map(|_| mempool.sampled_transactions),
            "observed_at": mempool.observed_at,
            "fresh": mempool_fresh,
        }
    })))
}

fn provider_status(state: &AppState) -> Value {
    state.providers.as_ref().map_or(
        Value::Null,
        |(chain, mempool)| json!({"chain": chain.status(), "mempool": mempool.status()}),
    )
}

async fn providers(State(state): State<AppState>) -> Json<Value> {
    Json(provider_status(&state))
}

async fn ready(state: State<AppState>) -> Result<Response, ApiError> {
    let Json(value) = status(state).await?;
    let code = if value["ready"] == true {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    Ok((code, Json(value)).into_response())
}
#[derive(Deserialize)]
struct Pagination {
    #[serde(default)]
    from: u64,
    #[serde(default = "default_limit")]
    limit: u32,
    at: Option<Bytes32>,
}
fn default_limit() -> u32 {
    100
}
fn check_page(page: &Pagination) -> Result<(), ApiError> {
    if !(1..=1000).contains(&page.limit) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "limit must be between 1 and 1000",
        ));
    }
    Ok(())
}
async fn leaves(
    State(state): State<AppState>,
    Query(page): Query<Pagination>,
) -> Result<Json<Value>, ApiError> {
    check_page(&page)?;
    if page.from > tree::CAPACITY {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "from exceeds tree capacity",
        ));
    }
    Ok(Json(json!(
        state.store.leaves(page.from, page.limit, page.at).await?
    )))
}
async fn blocks(
    State(state): State<AppState>,
    Query(page): Query<Pagination>,
) -> Result<Json<Value>, ApiError> {
    check_page(&page)?;
    let from = u32::try_from(page.from)
        .map_err(|_| ApiError(StatusCode::BAD_REQUEST, "height exceeds u32"))?;
    Ok(Json(json!(
        state.store.blocks(from, page.limit, page.at).await?
    )))
}
#[derive(Default, Deserialize)]
struct At {
    at: Option<Bytes32>,
}
async fn node(
    State(state): State<AppState>,
    Path((level, index)): Path<(u8, u64)>,
    Query(at): Query<At>,
) -> Result<Json<Value>, ApiError> {
    if level > tree::DEPTH || index >= (1u64 << (tree::DEPTH - level)) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "node coordinates outside depth-32 tree",
        ));
    }
    Ok(Json(json!(state.store.node(level, index, at.at).await?)))
}
async fn nullifier(
    State(state): State<AppState>,
    Path(value): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let value = value
        .parse()
        .map_err(|_| ApiError(StatusCode::BAD_REQUEST, "nullifier must be 32 bytes of hex"))?;
    let (snapshot, leaf) = state.store.nullifier(value).await?;
    Ok(Json(
        json!({"snapshot":snapshot, "found":leaf.is_some(), "leaf":leaf}),
    ))
}
#[derive(Deserialize)]
struct MempoolPage {
    #[serde(default)]
    from: usize,
    #[serde(default = "default_limit")]
    limit: u32,
    sequence: Option<u64>,
}
async fn mempool(
    State(state): State<AppState>,
    Query(page): Query<MempoolPage>,
) -> Result<Json<Value>, ApiError> {
    if !(1..=1000).contains(&page.limit) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "limit must be between 1 and 1000",
        ));
    }
    let snapshot = state.mempool.read().await;
    if page.sequence.is_some_and(|s| s != snapshot.sequence) {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "mempool changed; restart pagination",
        ));
    }
    let items: Vec<_> = snapshot
        .actions
        .iter()
        .skip(page.from)
        .take(page.limit as usize)
        .collect();
    let next = page.from.saturating_add(items.len());
    Ok(Json(json!({
        "sequence":snapshot.sequence, "observed_at":snapshot.observed_at,
        "fresh":!snapshot.failed && fresh(snapshot.observed_at, state.stale_after_seconds),
        "sampled_transactions":snapshot.sampled_transactions, "disappeared_during_sample":snapshot.disappeared_during_sample,
        "total_actions":snapshot.actions.len(), "items":items, "next": if next < snapshot.actions.len() { Some(next) } else { None }, "confirmed":false
    })))
}

//! Small embedded dashboard; uses the same origin and process as the read API.
use crate::api::AppState;
use axum::{
    Router,
    http::header,
    response::{IntoResponse, Response},
    routing::get,
};

fn asset(content_type: &'static str, content: impl IntoResponse) -> Response {
    ([
        (header::CONTENT_TYPE, content_type),
        (header::CACHE_CONTROL, "no-cache"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::CONTENT_SECURITY_POLICY, "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'"),
    ], content).into_response()
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/ironwood", get(ironwood))
        .route("/ironwood/", get(ironwood))
        .route(
            "/favicon.svg",
            get(|| async { asset("image/svg+xml", include_str!("../web/favicon.svg")) }),
        )
        .route(
            "/apple-touch-icon.png",
            get(|| async {
                asset(
                    "image/png",
                    include_bytes!("../web/apple-touch-icon.png").as_slice(),
                )
            }),
        )
        .route(
            "/share.png",
            get(|| async { asset("image/png", include_bytes!("../web/share.png").as_slice()) }),
        )
        .route(
            "/",
            get(|| async {
                asset(
                    "text/html; charset=utf-8",
                    include_str!("../web/index.html"),
                )
            }),
        )
        .route(
            "/dashboard.css",
            get(|| async {
                asset(
                    "text/css; charset=utf-8",
                    include_str!("../web/dashboard.css"),
                )
            }),
        )
        .route(
            "/dashboard.js",
            get(|| async {
                asset(
                    "text/javascript; charset=utf-8",
                    include_str!("../web/dashboard.js"),
                )
            }),
        )
        .route(
            "/tree.js",
            get(|| async {
                asset(
                    "text/javascript; charset=utf-8",
                    include_str!("../web/tree.js"),
                )
            }),
        )
        .route(
            "/tree-model.mjs",
            get(|| async {
                asset(
                    "text/javascript; charset=utf-8",
                    include_str!("../web/tree-model.mjs"),
                )
            }),
        )
}

async fn ironwood() -> Response {
    asset(
        "text/html; charset=utf-8",
        include_str!("../web/ironwood.html"),
    )
}

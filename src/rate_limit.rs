//! A bounded, per-connection-IP token bucket for the read API.
use axum::{
    Json,
    extract::{ConnectInfo, Request, State},
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv6Addr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const PER_SECOND: f64 = 20.0;
const BURST: u32 = 240;
const MAX_CLIENTS: usize = 4096;
const IDLE: Duration = Duration::from_secs(60);

struct Bucket {
    tokens: f64,
    updated: Instant,
}

struct Clients {
    buckets: HashMap<IpAddr, Bucket>,
    swept: Instant,
}

impl Clients {
    fn new(now: Instant) -> Self {
        Self {
            buckets: HashMap::new(),
            swept: now,
        }
    }

    fn allow(&mut self, ip: IpAddr, now: Instant) -> Result<(), u64> {
        // Sweep at most once per minute; requests cannot grow the map without bound.
        if now.saturating_duration_since(self.swept) >= IDLE {
            self.buckets
                .retain(|_, bucket| now.saturating_duration_since(bucket.updated) < IDLE);
            self.swept = now;
        }
        if self.buckets.len() >= MAX_CLIENTS && !self.buckets.contains_key(&ip) {
            return Err(IDLE.as_secs());
        }
        let bucket = self.buckets.entry(ip).or_insert(Bucket {
            tokens: f64::from(BURST),
            updated: now,
        });
        bucket.tokens = (bucket.tokens
            + now.saturating_duration_since(bucket.updated).as_secs_f64() * PER_SECOND)
            .min(f64::from(BURST));
        bucket.updated = now;
        if bucket.tokens < 1.0 {
            return Err(1);
        }
        bucket.tokens -= 1.0;
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct RateLimiter(Arc<Mutex<Clients>>);

impl Default for RateLimiter {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(Clients::new(Instant::now()))))
    }
}

pub(crate) async fn check(
    State(limiter): State<RateLimiter>,
    request: Request,
    next: Next,
) -> Response {
    // Never trust client-supplied forwarding headers. A reverse proxy shares a bucket.
    // Embedders without ConnectInfo share one fallback bucket rather than bypassing limits.
    let ip = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|peer| peer.0.ip().to_canonical())
        .unwrap_or(IpAddr::V6(Ipv6Addr::UNSPECIFIED));
    let decision = limiter
        .0
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .allow(ip, Instant::now());
    if let Err(seconds) = decision {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [
                (header::RETRY_AFTER, seconds.to_string()),
                (header::CACHE_CONTROL, "no-store".to_owned()),
            ],
            Json(serde_json::json!({ "error": "rate_limited", "retry_after_seconds": seconds })),
        )
            .into_response();
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, middleware, routing::get};
    use std::net::Ipv4Addr;
    use tower::ServiceExt;

    #[test]
    fn refresh_bursts_are_allowed_and_sustained_hammering_is_limited() {
        let start = Instant::now();
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let mut clients = Clients::new(start);
        for _ in 0..BURST {
            assert_eq!(clients.allow(ip, start), Ok(()));
        }
        assert_eq!(clients.allow(ip, start), Err(1));
        let later = start + Duration::from_secs(1);
        for _ in 0..20 {
            assert_eq!(clients.allow(ip, later), Ok(()));
        }
        assert_eq!(clients.allow(ip, later), Err(1));
        assert_eq!(
            clients.allow(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), later),
            Ok(())
        );
    }

    #[test]
    fn client_tracking_is_bounded_and_idle_entries_expire() {
        let start = Instant::now();
        let mut clients = Clients::new(start);
        for n in 0..MAX_CLIENTS {
            assert_eq!(
                clients.allow(IpAddr::V4(Ipv4Addr::from(n as u32)), start),
                Ok(())
            );
        }
        let new_ip = IpAddr::V4(Ipv4Addr::from(MAX_CLIENTS as u32));
        assert_eq!(clients.allow(new_ip, start), Err(60));
        assert_eq!(clients.buckets.len(), MAX_CLIENTS);
        assert_eq!(clients.allow(new_ip, start + IDLE), Ok(()));
        assert_eq!(clients.buckets.len(), 1);
    }

    #[tokio::test]
    async fn middleware_ignores_forged_headers_and_returns_a_retry_delay() {
        let limiter = RateLimiter::default();
        let app = Router::new()
            .route("/v1/status", get(|| async { "ok" }))
            .route_layer(middleware::from_fn_with_state(limiter.clone(), check))
            .route("/", get(|| async { "UI" }));
        for index in 0..3 {
            // Start with an exhausted bucket; don't depend on how fast the test runs.
            limiter.0.lock().unwrap().buckets.insert(
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                Bucket {
                    tokens: 0.0,
                    updated: Instant::now() + Duration::from_secs(60),
                },
            );
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/v1/status")
                        .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 1234))))
                        .header("x-forwarded-for", format!("192.0.2.{}", index % 255))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
            assert_eq!(response.headers()[header::RETRY_AFTER], "1");
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        }
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 1234))))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}

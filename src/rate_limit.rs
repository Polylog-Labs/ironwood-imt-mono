//! Bounded token buckets keyed by the verified client IP.
use axum::{
    Json,
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv6Addr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;

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

/// Configure only proxies you control. No forwarding headers are trusted by default.
#[derive(Default)]
pub struct ProxyConfig {
    /// Exact connection IPs of proxies that overwrite X-Real-IP from the socket peer.
    pub trusted_proxies: Vec<IpAddr>,
    /// Secret injected by this project's Vercel request transform, never by the browser.
    pub token: Option<String>,
}

impl ProxyConfig {
    fn client_ip(&self, request: &Request) -> Result<IpAddr, ()> {
        let peer = request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|peer| peer.0.ip().to_canonical());
        // Vercel overwrites this IP header. Authenticate our own deployment before
        // trusting it; the origin remains publicly reachable by other clients.
        let authenticated = self.token.as_ref().is_some_and(|token| {
            token.len() >= 32
                && single_header(request.headers(), "x-ironwood-proxy-token")
                    .is_some_and(|value| bool::from(token.as_bytes().ct_eq(value.as_bytes())))
        });
        if authenticated {
            return header_ip(request.headers(), "x-vercel-forwarded-for").ok_or(());
        }
        if peer.is_some_and(|ip| {
            self.trusted_proxies
                .iter()
                .any(|proxy| proxy.to_canonical() == ip)
        }) {
            return header_ip(request.headers(), "x-real-ip").ok_or(());
        }
        // Embedders without ConnectInfo share a fallback bucket, never a bypass.
        Ok(peer.unwrap_or(IpAddr::V6(Ipv6Addr::UNSPECIFIED)))
    }
}

fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(value)
}

fn header_ip(headers: &HeaderMap, name: &str) -> Option<IpAddr> {
    single_header(headers, name)?
        .parse::<IpAddr>()
        .ok()
        .map(|ip| ip.to_canonical())
}

#[derive(Clone)]
pub(crate) struct RateLimiter {
    clients: Arc<Mutex<Clients>>,
    proxy: Arc<ProxyConfig>,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new(ProxyConfig::default())
    }
}

impl RateLimiter {
    pub(crate) fn new(proxy: ProxyConfig) -> Self {
        Self {
            clients: Arc::new(Mutex::new(Clients::new(Instant::now()))),
            proxy: Arc::new(proxy),
        }
    }
}

pub(crate) async fn check(
    State(limiter): State<RateLimiter>,
    request: Request,
    next: Next,
) -> Response {
    let ip = match limiter.proxy.client_ip(&request) {
        Ok(ip) => ip,
        Err(()) => {
            return (
                StatusCode::BAD_REQUEST,
                [(header::CACHE_CONTROL, "no-store")],
                Json(serde_json::json!({"error": "invalid_proxy_client_ip"})),
            )
                .into_response();
        }
    };
    let decision = limiter
        .clients
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

    fn proxy_request(client: &str, token: Option<&str>) -> Request {
        let mut request = Request::builder()
            .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 1234))))
            .header("x-vercel-forwarded-for", client);
        if let Some(token) = token {
            request = request.header("x-ironwood-proxy-token", token);
        }
        request.body(Body::empty()).unwrap()
    }

    #[test]
    fn one_hundred_visitors_behind_one_proxy_have_independent_bursts() {
        let token = "test-only-proxy-secret-32-characters";
        let proxy = ProxyConfig {
            token: Some(token.into()),
            ..Default::default()
        };
        let now = Instant::now();
        let mut clients = Clients::new(now);
        for visitor in 1..=100 {
            let ip = proxy
                .client_ip(&proxy_request(&format!("192.0.2.{visitor}"), Some(token)))
                .unwrap();
            for _ in 0..BURST {
                assert_eq!(clients.allow(ip, now), Ok(()));
            }
            assert_eq!(clients.allow(ip, now), Err(1));
        }
        assert_eq!(clients.buckets.len(), 100);
    }

    #[test]
    fn proxy_identity_requires_authentication_and_an_unambiguous_ip() {
        let token = "test-only-proxy-secret-32-characters";
        let proxy = ProxyConfig {
            token: Some(token.into()),
            ..Default::default()
        };
        let peer = IpAddr::V4(Ipv4Addr::LOCALHOST);
        for supplied in [None, Some("wrong-secret")] {
            assert_eq!(
                proxy.client_ip(&proxy_request("192.0.2.1", supplied)),
                Ok(peer)
            );
        }
        for ip in ["", "invalid", "192.0.2.1, 192.0.2.2", "192.0.2.1:443"] {
            assert_eq!(proxy.client_ip(&proxy_request(ip, Some(token))), Err(()));
        }
        let mut duplicate = proxy_request("192.0.2.1", Some(token));
        duplicate
            .headers_mut()
            .append("x-vercel-forwarded-for", "192.0.2.2".parse().unwrap());
        assert_eq!(proxy.client_ip(&duplicate), Err(()));
        for ip in ["192.0.2.1", "::ffff:192.0.2.1"] {
            assert_eq!(
                proxy.client_ip(&proxy_request(ip, Some(token))),
                Ok("192.0.2.1".parse().unwrap())
            );
        }
        assert_eq!(
            proxy.client_ip(&proxy_request("2001:db8::1", Some(token))),
            Ok("2001:db8::1".parse().unwrap())
        );
        let trusted = ProxyConfig {
            trusted_proxies: vec![peer],
            ..Default::default()
        };
        let mut request = proxy_request("192.0.2.99", None);
        assert_eq!(trusted.client_ip(&request), Err(()));
        request
            .headers_mut()
            .insert("x-real-ip", "192.0.2.1".parse().unwrap());
        assert_eq!(
            trusted.client_ip(&request),
            Ok("192.0.2.1".parse().unwrap())
        );
        assert_eq!(ProxyConfig::default().client_ip(&request), Ok(peer));
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
            limiter.clients.lock().unwrap().buckets.insert(
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

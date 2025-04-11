use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::handler::HandlerWithoutStateExt;
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{BoxError, Router, http};
use axum_client_ip::{ClientIp, ClientIpSource};
use axum_extra::extract::Host;
use axum_response_cache::CacheLayer;
use clap::Parser;
use rustls_acme::AcmeConfig;
use rustls_acme::caches::DirCache;
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_stream::StreamExt;
use tower::ServiceBuilder;
use tower_http::compression::CompressionLayer;
use tower_http::trace::TraceLayer;
use tracing::Span;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

const MAP_URL: &str = "https://api.chatwars.me/webview/map";
const LOCAL_HOST: &str = "https://maratik.fyi";
const MAP_ROUTE: &str = "/api/chatwars/webview/map";

static ALLOWED_ORIGINS: LazyLock<HashSet<&str>> = LazyLock::new(|| {
    HashSet::from([
        "https://maratik123.github.io",
        "http://127.0.0.1:8080",
        "http://[::1]:8080",
        "http://localhost:8080",
        "http://localhost.:8080",
    ])
});

#[derive(Parser, Debug)]
struct Args {
    /// Domains
    #[clap(short, required = true)]
    domains: Vec<String>,

    /// Contact info
    #[clap(short)]
    email: Vec<String>,

    /// Cert cache directory
    #[clap(short)]
    cache: Option<PathBuf>,

    /// Use Let's Encrypt production environment
    /// (see https://letsencrypt.org/docs/staging-environment/)
    #[clap(long)]
    prod: bool,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                concat!(env!("CARGO_CRATE_NAME"), "=debug,tower_http=debug").into()
            }),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    tokio::spawn(redirect_http_to_https());

    let client = reqwest::Client::new();

    let mut state = AcmeConfig::new(args.domains)
        .contact(args.email.into_iter().map(|e| format!("mailto:{e}")))
        .cache_option(args.cache.map(DirCache::new))
        .directory_lets_encrypt(args.prod)
        .state();
    let acceptor = state.axum_acceptor(state.default_rustls_config());

    tokio::spawn(async move {
        loop {
            let state = state.next().await;
            match state.unwrap() {
                Ok(ok) => tracing::info!("event: {:?}", ok),
                Err(err) => tracing::error!("error: {:?}", err),
            }
        }
    });

    let app = Router::new()
        .route(MAP_ROUTE, get(stream_map_api_response))
        .layer(
            ServiceBuilder::new()
                .layer(ClientIpSource::ConnectInfo.into_extension())
                .layer(TraceLayer::new_for_http().on_body_chunk(
                    |chunk: &Bytes, _latency: Duration, _span: &Span| {
                        tracing::debug!("streaming {} bytes", chunk.len());
                    },
                ))
                .layer(CompressionLayer::new())
                .layer(
                    CacheLayer::with_lifespan(0)
                        .add_response_headers()
                        .use_stale_on_failure(),
                ),
        )
        .with_state(client.clone());

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        let url = format!("{LOCAL_HOST}{MAP_ROUTE}");

        loop {
            interval.tick().await;
            match client.get(url.as_str()).send().await {
                Ok(_) => tracing::info!("ping successful"),
                Err(_) => tracing::error!("ping failed"),
            }
        }
    });

    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 443);
    axum_server::bind(addr)
        .acceptor(acceptor)
        .serve(app.into_make_service_with_connect_info::<SocketAddr>())
        .await
        .unwrap();
}

async fn stream_map_api_response(
    header_map: HeaderMap,
    State(client): State<reqwest::Client>,
    ClientIp(ip): ClientIp,
) -> Response {
    tracing::info!(%ip, "request received");
    common_proxy_response(client.get(MAP_URL).send().await, header_map)
}

fn common_proxy_response(
    response: Result<reqwest::Response, reqwest::Error>,
    header_map: HeaderMap,
) -> Response {
    let map_api_response = match response {
        Ok(res) => res,
        Err(err) => {
            tracing::error!(%err, "request failed");
            return (StatusCode::BAD_REQUEST, Body::empty()).into_response();
        }
    };

    let mut response_builder = Response::builder().status(map_api_response.status());
    if let Some(headers) = response_builder.headers_mut() {
        *headers = map_api_response.headers().clone();
        headers.remove(http::header::COOKIE);
        if let Some(origin_value) = header_map
            .get_all(http::header::ORIGIN)
            .into_iter()
            .filter_map(|val| val.to_str().ok())
            .find_map(|val| ALLOWED_ORIGINS.get(val))
        {
            headers.insert(
                http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
                HeaderValue::from_static(origin_value),
            );
        }
    }
    response_builder
        .body(Body::from_stream(map_api_response.bytes_stream()))
        // This unwrap is fine because the body is empty here
        .unwrap()
}

async fn redirect_http_to_https() {
    fn make_https(host: String, uri: Uri) -> Result<Uri, BoxError> {
        let mut parts = uri.into_parts();

        parts.scheme = Some(http::uri::Scheme::HTTPS);

        if parts.path_and_query.is_none() {
            parts.path_and_query = Some("/".parse().unwrap());
        }

        let https_host = host.replace("80", "443");
        parts.authority = Some(https_host.parse()?);

        Ok(Uri::from_parts(parts)?)
    }

    let redirect = move |Host(host): Host, uri: Uri| async move {
        match make_https(host, uri) {
            Ok(uri) => Ok(Redirect::permanent(&uri.to_string())),
            Err(error) => {
                tracing::warn!(%error, "failed to convert URI to HTTPS");
                Err(StatusCode::BAD_REQUEST)
            }
        }
    };

    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 80);
    let listener = TcpListener::bind(addr).await.unwrap();
    tracing::debug!("listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, redirect.into_make_service())
        .await
        .unwrap();
}

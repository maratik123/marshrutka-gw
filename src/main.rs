use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use reqwest::Client;
use std::time::Duration;
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;
use tracing::Span;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

const MAP_URL: &str = "https://api.chatwars.me/webview/map";
const MARSHRUTKA_ORIGIN: &str = "https://maratik123.github.io";

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                format!("{}=debug,tower_http=debug", env!("CARGO_CRATE_NAME")).into()
            }),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let client = Client::new();

    let app = Router::new()
        .route("/api/chatwars/webview/map", get(stream_map_api_response))
        .layer(TraceLayer::new_for_http().on_body_chunk(
            |chunk: &Bytes, _latency: Duration, _span: &Span| {
                tracing::debug!("streaming {} bytes", chunk.len());
            },
        ))
        .with_state(client);

    let listener = TcpListener::bind("0.0.0.0:80").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn stream_map_api_response(State(client): State<Client>) -> Response {
    let map_api_response = match client.get(MAP_URL).send().await {
        Ok(res) => res,
        Err(err) => {
            tracing::error!(%err, "request failed");
            return (StatusCode::BAD_REQUEST, Body::empty()).into_response();
        }
    };

    let mut response_builder = Response::builder().status(map_api_response.status());
    {
        let headers = response_builder.headers_mut().unwrap();
        *headers = map_api_response.headers().clone();
        headers.insert(
            "Access-Control-Allow-Origin",
            HeaderValue::from_static(MARSHRUTKA_ORIGIN),
        );
    }
    response_builder
        .body(Body::from_stream(map_api_response.bytes_stream()))
        // This unwrap is fine because the body is empty here
        .unwrap()
}

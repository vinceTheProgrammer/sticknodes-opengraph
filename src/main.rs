use std::{net::{IpAddr, SocketAddr}, sync::Arc, time::Duration};

use axum::{
    Router, debug_handler, error_handling::HandleErrorLayer, extract::{OriginalUri, State}, http::{StatusCode, header}, response::{IntoResponse, Response}, routing::get
};
use axum::body::Body;
use reqwest::{Url, header::CONTENT_TYPE};
use snrs_render_core::compile_frame;
use sticknodes_rs::Stickfigure;
use tokio::{net::{TcpListener, lookup_host}, sync::Semaphore};
use base64::prelude::*;
use futures_util::StreamExt;
use tower::{BoxError, ServiceBuilder};
use tower_governor::{GovernorLayer, governor::GovernorConfigBuilder};

use crate::utils::render::render_to_png;

mod utils;

const MAX_SIZE: u64 = 5 * 1024 * 1024; // 5 MB
const ALLOWED_HOSTS: &[&str] = &[
    // "raw.githubusercontent.com",
    "sticknodes.com",
];

#[derive(Clone)]
struct AppState {
    semaphore: Arc<Semaphore>,
}

#[tokio::main]
async fn main() {
    let state = AppState {
        semaphore: Arc::new(Semaphore::new(16)),
    };

    let governor_conf = Arc::new(
        GovernorConfigBuilder::default()
            .per_second(1)      // 1 request per second per IP
            .burst_size(5)      // allow bursts of 5
            .finish()
            .unwrap()
    );

    let app = Router::new()
    .route("/*path", get(handle))
    .with_state(state)
    .layer(
        ServiceBuilder::new()
            .layer(GovernorLayer {
                config: governor_conf
            })
    );

    let listener = TcpListener::bind("127.0.0.1:3000").await.unwrap();

    println!("Listening on {}", listener.local_addr().unwrap());

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    .unwrap();
}

fn is_ip_allowed(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.octets()[0] == 0)
        }
        IpAddr::V6(v6) => {
            !(v6.is_loopback()
                || v6.is_multicast()
                || v6.is_unspecified()
                || v6.is_unique_local()
                || v6.is_unicast_link_local())
        }
    }
}

async fn handle_governor_error(err: BoxError) -> impl IntoResponse {
    if err.is::<tower_governor::errors::GovernorError>() {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "Rate limit exceeded",
        );
    }

    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "Unhandled internal error",
    )
}

#[debug_handler]
async fn handle(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response, StatusCode> {
    // Acquire concurrency permit
    let _permit = state.semaphore
        .acquire()
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;

    let path = uri.path();
    let target_url = path.strip_prefix('/')
        .ok_or(StatusCode::BAD_REQUEST)?;

        let url = Url::parse(target_url)
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    
    // Enforce HTTPS only
    if url.scheme() != "https" {
        return Err(StatusCode::BAD_REQUEST);
    }
    
    // Hostname required
    let host = url.host_str()
        .ok_or(StatusCode::BAD_REQUEST)?;
    
    // Exact whitelist match
    if !ALLOWED_HOSTS.contains(&host) {
        return Err(StatusCode::FORBIDDEN);
    }
    
    // Resolve DNS manually to prevent DNS rebinding
    let port = url.port_or_known_default()
        .ok_or(StatusCode::BAD_REQUEST)?;
    
    let addrs: Vec<SocketAddr> = lookup_host((host, port))
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?
        .collect();
    
    if addrs.is_empty() {
        return Err(StatusCode::BAD_GATEWAY);
    }
    
    // Ensure all resolved IPs are safe
    for addr in &addrs {
        if !is_ip_allowed(addr.ip()) {
            return Err(StatusCode::FORBIDDEN);
        }
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none()) // important
        .build()
        .map_err(|_| StatusCode::BAD_GATEWAY)?;

    let resp = client.get(target_url)
        .send()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;

    if !resp.status().is_success() {
        return Err(StatusCode::BAD_GATEWAY);
    }

    if let Some(len) = resp.content_length() {
        if len > MAX_SIZE {
            return Err(StatusCode::PAYLOAD_TOO_LARGE);
        }
    }

    let content_type = resp.headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());

    if content_type != Some("application/octet-stream") {
        return Err(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    let mut stream = resp.bytes_stream();
    let mut total: u64 = 0;
    let mut buffer = Vec::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| StatusCode::BAD_GATEWAY)?;
        total += chunk.len() as u64;

        if total > MAX_SIZE {
            return Err(StatusCode::PAYLOAD_TOO_LARGE);
        }

        buffer.extend_from_slice(&chunk);
    }

    let compiled = {
        let stickfigure = Stickfigure::from_bytes(buffer)
        .map_err(|_| StatusCode::UNPROCESSABLE_ENTITY)?;

        compile_frame(&stickfigure)
    };

    let png_bytes = render_to_png(&compiled)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let base64_img = BASE64_STANDARD.encode(&png_bytes);

    let html = format!(
        r#"
        <!DOCTYPE html>
        <html lang="en">
        <head>
            <meta charset="UTF-8">
            <meta property="og:title" content="Rendered Stickfigure" />
            <meta property="og:image" content="data:image/png;base64,{base64_img}" />
            <meta property="og:type" content="website" />
        </head>
        <body style="margin:0; background:#111; display:flex; justify-content:center; align-items:center; min-height:100vh;">
            <img src="data:image/png;base64,{base64_img}" style="max-width:90vw; max-height:90vh;" />
        </body>
        </html>
        "#
    );

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(html))
        .unwrap())
}
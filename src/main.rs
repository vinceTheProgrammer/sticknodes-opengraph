use std::{net::{IpAddr, SocketAddr}, sync::Arc, time::Duration};
use std::panic::{AssertUnwindSafe, catch_unwind};
use axum::{
    Router, debug_handler, extract::{OriginalUri, State}, http::{StatusCode, header}, response::Response, routing::get
};
use axum::body::Body;
use reqwest::{Url, header::CONTENT_TYPE};
use snrs_render_core::compile_frame;
use sticknodes_rs::Stickfigure;
use tokio::{net::{TcpListener, lookup_host}, sync::Semaphore};
use base64::prelude::*;
use futures_util::StreamExt;
use tower::ServiceBuilder;
use tower_governor::{GovernorLayer, governor::GovernorConfigBuilder};

use crate::utils::render::render_to_png;

mod utils;

const MAX_SIZE: u64 = 5 * 1024 * 1024; // 5 MB
const ALLOWED_HOSTS: &[&str] = &[
    "raw.githubusercontent.com",
    "sticknodes.com",
    "cdn.discordapp.com",
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

#[debug_handler]
async fn handle(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response, StatusCode> {

    println!("---- NEW REQUEST ----");
    println!("URI: {}", uri);

    // Acquire concurrency permit
    let _permit = match state.semaphore.acquire().await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Semaphore acquire failed: {e}");
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }
    };

    let path = uri.path();
    println!("Path: {path}");

    let target_url = match path.strip_prefix('/') {
        Some(p) => p,
        None => {
            eprintln!("Missing leading slash");
            return Err(StatusCode::BAD_REQUEST);
        }
    };

    println!("Target URL raw: {target_url}");

    let url = match Url::parse(target_url) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("URL parse failed: {e}");
            return Err(StatusCode::BAD_REQUEST);
        }
    };

    if url.scheme() != "https" {
        eprintln!("Rejected non-https scheme: {}", url.scheme());
        return Err(StatusCode::BAD_REQUEST);
    }

    let host = match url.host_str() {
        Some(h) => h,
        None => {
            eprintln!("No host in URL");
            return Err(StatusCode::BAD_REQUEST);
        }
    };

    println!("Host: {host}");

    if !ALLOWED_HOSTS.contains(&host) {
        eprintln!("Host not allowed: {host}");
        return Err(StatusCode::FORBIDDEN);
    }

    let port = match url.port_or_known_default() {
        Some(p) => p,
        None => {
            eprintln!("No port resolved");
            return Err(StatusCode::BAD_REQUEST);
        }
    };

    println!("Resolving {host}:{port}");

    let addrs: Vec<SocketAddr> = match lookup_host((host, port)).await {
        Ok(a) => a.collect(),
        Err(e) => {
            eprintln!("DNS lookup failed: {e}");
            return Err(StatusCode::BAD_GATEWAY);
        }
    };

    if addrs.is_empty() {
        eprintln!("DNS returned no addresses");
        return Err(StatusCode::BAD_GATEWAY);
    }

    for addr in &addrs {
        println!("Resolved IP: {}", addr.ip());
        if !is_ip_allowed(addr.ip()) {
            eprintln!("Rejected IP: {}", addr.ip());
            return Err(StatusCode::FORBIDDEN);
        }
    }

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Client build failed: {e}");
            return Err(StatusCode::BAD_GATEWAY);
        }
    };

    println!("Fetching remote file...");

    let resp = match client
    .get(target_url)
    .header("User-Agent",
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
         AppleWebKit/537.36 (KHTML, like Gecko) \
         Chrome/122.0.0.0 Safari/537.36")
    .header("Accept", "*/*")
    .header("Referer", "https://sticknodes.com/")
    .header("Accept-Language", "en-US,en;q=0.9")
    .header("Accept-Encoding", "gzip, deflate, br")
    .header("Connection", "keep-alive")
    .header("sec-fetch-dest", "image")
    .header("sec-fetch-mode", "no-cors")
    .header("sec-fetch-site", "cross-site")
    .send()
    .await
{
        Ok(r) => r,
        Err(e) => {
            eprintln!("HTTP request failed: {e}");
            return Err(StatusCode::BAD_GATEWAY);
        }
    };

    println!("Remote status: {}", resp.status());

    if !resp.status().is_success() {
        eprintln!("Remote returned non-success: {}", resp.status());
        return Err(StatusCode::BAD_GATEWAY);
    }

    if let Some(len) = resp.content_length() {
        println!("Content-Length: {len}");
        if len > MAX_SIZE {
            eprintln!("File too large: {len}");
            return Err(StatusCode::PAYLOAD_TOO_LARGE);
        }
    }

    let content_type = resp.headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());

    println!("Content-Type: {:?}", content_type);

    if content_type != Some("application/octet-stream") {
        eprintln!("Unsupported content type: {:?}", content_type);
        return Err(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    println!("Streaming body...");

    let mut stream = resp.bytes_stream();
    let mut total: u64 = 0;
    let mut buffer = Vec::new();

    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Stream error: {e}");
                return Err(StatusCode::BAD_GATEWAY);
            }
        };

        total += chunk.len() as u64;

        if total > MAX_SIZE {
            eprintln!("Exceeded max size during stream: {total}");
            return Err(StatusCode::PAYLOAD_TOO_LARGE);
        }

        buffer.extend_from_slice(&chunk);
    }

    println!("Downloaded {} bytes", total);

    println!("Parsing Stickfigure...");

    let compiled = match catch_unwind(AssertUnwindSafe(|| {
        let stickfigure = Stickfigure::from_bytes(buffer)
            .expect("Stickfigure::from_bytes failed");

        compile_frame(&stickfigure)
    })) {
        Ok(c) => c,
        Err(_) => {
            eprintln!("compile_frame panicked!");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

    println!("Rendering PNG...");

    let png_bytes = match render_to_png(&compiled).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("render_to_png failed: {:?}", e);
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

    println!("PNG size: {}", png_bytes.len());

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

    println!("Returning OK response");

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(html))
        .unwrap())
}
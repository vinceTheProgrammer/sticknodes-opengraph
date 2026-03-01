use std::{
    path::PathBuf,
    sync::Arc,
};

use axum::{
    Json, Router, body::Body, debug_handler, extract::{Multipart, Path as AxumPath, State}, http::{StatusCode, header}, response::{Html, Response}, routing::get
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::{fs, sync::Semaphore};
use tower::ServiceBuilder;
use tower_governor::{governor::GovernorConfigBuilder, GovernorLayer};

use snrs_render_core::compile_frame;
use sticknodes_rs::Stickfigure;

use crate::utils::render::{render_to_png, Renderer};

mod utils;

const MAX_SIZE: usize = 5 * 1024 * 1024; // 5MB

#[derive(Clone)]
struct AppState {
    renderer: Arc<Renderer>,
    semaphore: Arc<Semaphore>,
    data_dir: PathBuf,
}

#[tokio::main]
async fn main() {
    let renderer = Arc::new(Renderer::new().await.unwrap());

    let data_dir = std::env::var("RENDER_DATA_DIR")
    .unwrap_or_else(|_| "/var/lib/render-service".to_string());

    let data_dir = PathBuf::from(data_dir);
    
    fs::create_dir_all(data_dir.join("nodes")).await.unwrap();
    fs::create_dir_all(data_dir.join("png")).await.unwrap();

    let state = AppState {
        renderer,
        semaphore: Arc::new(Semaphore::new(16)),
        data_dir,
    };

    let governor_conf = Arc::new(
        GovernorConfigBuilder::default()
            .per_second(2)
            .burst_size(5)
            .finish()
            .unwrap(),
    );

    let app = Router::new()
        .route("/", get(root_page).post(upload_nodes))
        .route("/p/:id", get(og_page))
        .route("/p/:id/png", get(get_png))
        .with_state(state)
        .layer(ServiceBuilder::new().layer(GovernorLayer {
            config: governor_conf,
        }));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000")
        .await
        .unwrap();

    println!("Listening on http://127.0.0.1:3000");

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    .unwrap();
}

async fn root_page() -> Html<&'static str> {
    Html(r#"<!DOCTYPE html>
<html>
<head>
<meta charset="UTF-8">
<title>Sticknodes Renderer</title>
<style>
body {
    font-family: system-ui, sans-serif;
    background: #111;
    color: #eee;
    display: flex;
    justify-content: center;
    padding: 40px;
}
.container {
    max-width: 700px;
    width: 100%;
}
h1 {
    margin-bottom: 10px;
}
.card {
    background: #1e1e1e;
    padding: 20px;
    border-radius: 8px;
}
input[type="file"] {
    margin-top: 10px;
    margin-bottom: 15px;
    color: #ccc;
}
button {
    background: #4f46e5;
    border: none;
    padding: 8px 14px;
    color: white;
    border-radius: 4px;
    cursor: pointer;
    font-size: 14px;
}
button:hover {
    background: #6366f1;
}
.upload-area {
    border: 2px dashed #333;
    padding: 30px;
    text-align: center;
    border-radius: 8px;
    margin-bottom: 15px;
}
</style>
</head>
<body>
<div class="container">
<h1>Sticknodes Renderer</h1>

<div class="card">
<form method="POST" enctype="multipart/form-data">
<div class="upload-area">
<p>Select a <strong>.nodes</strong> file to render</p>
<input type="file" name="file" accept=".nodes" required />
</div>
<button type="submit">Render</button>
</form>
</div>

</div>
</body>
</html>"#)
}

#[derive(Serialize)]
struct UploadResponse {
    id: String,
    png_url: String,
    og_url: String,
}

#[debug_handler]
async fn upload_nodes(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Html<String>, StatusCode> {
    let _permit = state.semaphore.acquire().await.unwrap();

    let mut file_bytes = Vec::new();

    while let Some(field) = multipart.next_field().await.unwrap() {
        if field.name() == Some("file") {
            file_bytes = field.bytes().await.unwrap().to_vec();
            break;
        }
    }

    if file_bytes.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    if file_bytes.len() > MAX_SIZE {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }

    // Hash file
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(&file_bytes);
    let id = format!("{:x}", hasher.finalize());

    let nodes_path = state.data_dir.join("nodes").join(format!("{id}.nodes"));
    let png_path = state.data_dir.join("png").join(format!("{id}.png"));

    if !png_path.exists() {
        
        let compiled = {
            let stickfigure =
            Stickfigure::from_bytes(file_bytes.clone())
                .map_err(|_| StatusCode::BAD_REQUEST)?;
            compile_frame(&stickfigure)
        };

        let png_bytes =
            render_to_png(&state.renderer, &compiled)
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        tokio::fs::write(&nodes_path, &file_bytes)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        tokio::fs::write(&png_path, png_bytes)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }

    // Build absolute URLs
    let png_url = format!("/p/{id}/png");
    let og_url = format!("/p/{id}");

    let html = format!(r#"<!DOCTYPE html>
<html>
<head>
<meta charset="UTF-8">
<title>Render Complete</title>
<style>
body {{
    font-family: system-ui, sans-serif;
    background: #111;
    color: #eee;
    display: flex;
    justify-content: center;
    padding: 40px;
}}
.container {{
    max-width: 700px;
    width: 100%;
}}
h1 {{
    margin-bottom: 10px;
}}
img {{
    max-width: 100%;
    background: #222;
    padding: 10px;
    border-radius: 8px;
}}
.link-box {{
    background: #1e1e1e;
    padding: 10px;
    margin-top: 15px;
    border-radius: 6px;
    display: flex;
    justify-content: space-between;
    align-items: center;
    font-size: 14px;
}}
button {{
    background: #4f46e5;
    border: none;
    padding: 6px 10px;
    color: white;
    border-radius: 4px;
    cursor: pointer;
}}
button:hover {{
    background: #6366f1;
}}
a {{
    color: #93c5fd;
    text-decoration: none;
}}
</style>
<script>
function copyToClipboard(text) {{
    navigator.clipboard.writeText(text);
}}
</script>
</head>
<body>
<div class="container">
<h1>Render Complete ✅</h1>

<img src="{png_url}" />

<div class="link-box">
<span>{png_url}</span>
<button onclick="copyToClipboard(window.location.origin + '{png_url}')">Copy PNG</button>
</div>

<div class="link-box">
<span>{og_url}</span>
<button onclick="copyToClipboard(window.location.origin + '{og_url}')">Copy Share Link</button>
</div>

<br>
<a href="/">Upload another</a>

</div>
</body>
</html>"#);

    Ok(Html(html))
}

async fn get_png(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Response, StatusCode> {
    let path = state.data_dir.join("png").join(format!("{id}.png"));

    let bytes = fs::read(path).await.map_err(|_| StatusCode::NOT_FOUND)?;

    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "image/png")
        .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
        .body(Body::from(bytes))
        .unwrap())
}

async fn og_page(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Html<String>, StatusCode> {
    let png_path = state.data_dir.join("png").join(format!("{id}.png"));

    if !png_path.exists() {
        return Err(StatusCode::NOT_FOUND);
    }

    let png_url = format!("/p/{id}/png");

    let html = format!(
        r#"<!DOCTYPE html>
<html>
<head>
<meta property="og:title" content="Rendered Stickfigure" />
<meta property="og:image" content="{png_url}" />
<meta property="og:type" content="website" />
<meta property="og:image:type" content="image/png" />
</head>
<body>
<img src="{png_url}" />
</body>
</html>"#
    );

    Ok(Html(html))
}
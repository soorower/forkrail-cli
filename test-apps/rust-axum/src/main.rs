//! An axum web server on PORT, run on provided.al2023 with forkrail's runtime as its bootstrap.

use axum::{Json, Router, extract::Path, routing::{get, post}};
use serde_json::{Value, json};

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/", get(|| async { Json(json!({"framework": "axum", "arch": std::env::consts::ARCH})) }))
        .route("/echo", post(|body: String| async move {
            println!("echo {} bytes", body.len());
            body
        }))
        .route("/env/{name}", get(|Path(name): Path<String>| async move {
            let value = std::env::var(&name).ok();
            Json::<Value>(json!({"name": name, "value": value}))
        }));
    let port = std::env::var("PORT").unwrap_or_else(|_| "3000".to_owned());
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await.unwrap();
    eprintln!("listening on {port}");
    axum::serve(listener, app).await.unwrap();
}

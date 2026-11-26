//! 基准对照：axum 0.8 + tokio。端点与 rsboot 版本一字不差。

use axum::extract::Path;
use axum::http::header;
use axum::routing::get;
use axum::Router;

const BODY: &str = r#"{"ok":true,"n":1,"msg":"benchmark payload 0123456789abcdef"}"#;

static BIG: [u8; 64 * 1024] = [b'a'; 64 * 1024];

async fn big() -> impl axum::response::IntoResponse {
    ([(header::CONTENT_TYPE, "application/json")], &BIG[..])
}

async fn slow() -> impl axum::response::IntoResponse {
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    ([(header::CONTENT_TYPE, "application/json")], BODY)
}

async fn json() -> impl axum::response::IntoResponse {
    ([(header::CONTENT_TYPE, "application/json")], BODY)
}

async fn item(Path(id): Path<String>) -> impl axum::response::IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/json")],
        format!(r#"{{"id":"{id}"}}"#),
    )
}

#[tokio::main(worker_threads = 2)]
async fn main() {
    let bind = std::env::var("BIND").unwrap_or_else(|_| "127.0.0.1:9002".into());
    let app = Router::new()
        .route("/json", get(json))
        .route("/slow", get(slow))
        .route("/big", get(big))
        .route("/items/{id}", get(item));
    let l = tokio::net::TcpListener::bind(&bind).await.unwrap();
    println!("axum 监听 {bind}");
    axum::serve(l, app).await.unwrap();
}

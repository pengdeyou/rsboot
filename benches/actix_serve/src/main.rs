//! 基准对照：actix-web 4。端点与 rsboot 版本一致，且都不做 JSON 序列化
//! （预生成字节），这样比的是栈本身而不是序列化速度。

use actix_web::{http::header, web, App, HttpRequest, HttpResponse, HttpServer, Responder};

const BODY: &str = r#"{"ok":true,"n":1,"msg":"benchmark payload 0123456789abcdef"}"#;

static BIG: [u8; 64 * 1024] = [b'a'; 64 * 1024];

async fn big() -> impl Responder {
    actix_web::HttpResponse::Ok()
        .insert_header((header::CONTENT_TYPE, "application/json"))
        .body(&BIG[..])
}

async fn slow() -> impl Responder {
    actix_web::rt::time::sleep(std::time::Duration::from_millis(20)).await;
    HttpResponse::Ok()
        .insert_header((header::CONTENT_TYPE, "application/json"))
        .body(BODY)
}

async fn json() -> impl Responder {
    HttpResponse::Ok()
        .insert_header((header::CONTENT_TYPE, "application/json"))
        .body(BODY)
}

async fn item(req: HttpRequest) -> impl Responder {
    let id = req.match_info().get("id").unwrap_or("").to_string();
    HttpResponse::Ok()
        .insert_header((header::CONTENT_TYPE, "application/json"))
        .body(format!(r#"{{"id":"{id}"}}"#))
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let bind = std::env::var("BIND").unwrap_or_else(|_| "127.0.0.1:9003".into());
    println!("actix 监听 {bind}");
    HttpServer::new(|| {
        App::new()
            .route("/json", web::get().to(json))
            .route("/slow", web::get().to(slow))
            .route("/big", web::get().to(big))
            .route("/items/{id}", web::get().to(item))
    })
    .workers(2)
    .bind(bind)?
    .run()
    .await
}

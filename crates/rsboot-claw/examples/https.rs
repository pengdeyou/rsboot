//! 真实站点 HTTPS 冒烟：`cargo run --example https`
//!
//! 走的是本 crate 自研的 TLS 1.3（没有 rustls、没有 openssl），用来验证握手、
//! 证书链、应用数据收发在真实服务端上确实成立。
//!
//! 选站标准：覆盖不同的证书签名算法与套件 —— P-256 leaf、P-384 中间证书、
//! RSA-PSS / RSA-PKCS1 leaf、被旧根交叉签过的根。故意不收只会 TLS 1.2 的站点
//! （`www.baidu.com` 就是这类，服务端直接回 alert 70），那是已知边界。

use rsboot_claw::http::Client;
use rsboot_claw::url::Url;

fn main() {
    let sites = [
        "https://example.com/",
        "https://www.cloudflare.com/",
        "https://www.rust-lang.org/",
        "https://github.com/",
        "https://static.crates.io/",
    ];
    let mut client = Client::new();
    let mut ok = 0;
    for site in sites {
        let Some(url) = Url::parse(site) else {
            println!("{site}: bad url");
            continue;
        };
        match client.get(&url) {
            Ok(r) => {
                ok += 1;
                println!(
                    "{site}: {} {} bytes, server={}",
                    r.status,
                    r.body.len(),
                    r.header("server").unwrap_or("-")
                );
            }
            Err(e) => println!("{site}: ERROR {e}"),
        }
    }
    println!("{ok}/{} ok", sites.len());
}

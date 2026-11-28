//! `tls` feature 下的真站点验证。
//!
//! 默认 `#[ignore]`：跑测试不该依赖"这台机器此刻能访问某个外网 https 站点"。
//! 但它确实是唯一能证明"TLS 整条链路真的通了"的东西（自签证书的服务端
//! 要额外引 rcgen，为一条测试不值得）。
//!
//! ```sh
//! cargo test -p rsboot-claw --features tls --test tls_e2e -- --ignored --nocapture
//! ```
#![cfg(feature = "tls")]

use std::time::Duration;

use rsboot_claw::{Client, Url};

#[test]
#[ignore = "需要外网：只在开了 tls feature 且能访问外网时手动跑"]
fn https_fetches_a_real_site() {
    let mut c = Client::new().timeout(Duration::from_secs(10));
    let u = Url::parse("https://example.com/").unwrap();
    let r = c.get(&u).expect("https get");
    assert_eq!(r.status, 200, "{:?}", r.headers);
    assert!(r.text().to_ascii_lowercase().contains("example"));
    // 同一条 TLS 连接要能复用（否则每个页都要重握手，爬虫会明显变慢）
    let again = c.get(&u).expect("second get");
    assert_eq!(again.status, 200);
    assert_eq!(c.connects(), 1, "第二次应当复用同一条 TLS 连接");
}

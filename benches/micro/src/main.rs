//! 微观基准：把"每请求省下的纳秒"直接量出来，而不是泡在端到端 QPS 的噪声里。
//!
//! 端到端压测在这台 4 核 VM 上的波动有 20~30 %（93k~123k），
//! 几百纳秒的改动根本淹没在里面。这里用循环把单次开销放大，
//! 得到的是可以下判断的数字。
//!
//! 跑法：`cargo run --release`

use serde::Serialize;
use serde_json::Value;
use std::borrow::Cow;
use std::time::Instant;

#[derive(Serialize)]
struct Item {
    id: u64,
    name: &'static str,
    tags: Vec<&'static str>,
}

/// 改造前的统一响应体路径：to_value → json! 建外层树 → to_vec → 再 to_vec 拷贝
fn old_path(data: &Item, trace: Option<&str>) -> Vec<u8> {
    let mut o = serde_json::json!({
        "code": "OK",
        "message": "success",
        "data": serde_json::to_value(data).ok().unwrap_or(Value::Null),
    });
    if let Some(t) = trace {
        o["trace_id"] = serde_json::json!(t);
    }
    let bytes = serde_json::to_vec(&o).unwrap();
    // Response::json_with 的 `bytes.to_vec()`
    bytes.to_vec()
}

/// 改造后：一次分配，逐段写，Response 直接接管 buffer
fn new_path(data: &Item, trace: Option<&str>) -> Vec<u8> {
    let mut buf = Vec::with_capacity(128);
    buf.push(b'{');
    buf.extend_from_slice(br#""code":"#);
    serde_json::to_writer(&mut buf, "OK").unwrap();
    buf.extend_from_slice(br#","message":"#);
    serde_json::to_writer(&mut buf, "success").unwrap();
    buf.extend_from_slice(br#","data":"#);
    serde_json::to_writer(&mut buf, data).unwrap();
    if let Some(t) = trace {
        buf.extend_from_slice(br#","trace_id":"#);
        serde_json::to_writer(&mut buf, t).unwrap();
    }
    buf.push(b'}');
    buf
}

fn bench(label: &str, n: usize, mut f: impl FnMut() -> usize) -> f64 {
    // 预热，让分配器与分支预测进入稳态
    for _ in 0..1000 {
        std::hint::black_box(f());
    }
    let t = Instant::now();
    let mut sum = 0usize;
    for _ in 0..n {
        sum += f();
    }
    let el = t.elapsed();
    std::hint::black_box(sum);
    el.as_nanos() as f64 / n as f64
}

// ---------- 头部查找：一次请求里要查 5~8 次 ----------

fn old_get<'a>(items: &'a [(String, String)], name: &str) -> Option<&'a str> {
    let lower = name.to_ascii_lowercase();
    items.iter().find(|(n, _)| *n == lower).map(|(_, v)| v.as_str())
}

fn lower_name<'x>(name: &'x str, buf: &'x mut [u8; 40]) -> Cow<'x, str> {
    let b = name.as_bytes();
    if b.len() <= buf.len() {
        for (i, &c) in b.iter().enumerate() {
            buf[i] = c.to_ascii_lowercase();
        }
        match std::str::from_utf8(&buf[..b.len()]) {
            Ok(s) => Cow::Borrowed(s),
            Err(_) => Cow::Owned(name.to_ascii_lowercase()),
        }
    } else {
        Cow::Owned(name.to_ascii_lowercase())
    }
}

fn new_get<'a>(items: &'a [(String, String)], name: &str) -> Option<&'a str> {
    let mut buf = [0u8; 40];
    let lower = lower_name(name, &mut buf);
    items
        .iter()
        .find(|(n, _)| n.as_str() == lower.as_ref())
        .map(|(_, v)| v.as_str())
}

const LOOKUPS: [&str; 8] = [
    "Content-Length", "Host", "Connection", "Accept-Encoding",
    "User-Agent", "Accept", "content-type", "x-request-id",
];

fn headers() -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = Vec::with_capacity(10);
    for (k, val) in [
        ("host", "127.0.0.1:9001"),
        ("connection", "keep-alive"),
        ("accept-encoding", "gzip, deflate"),
        ("user-agent", "load/1.0"),
        ("accept", "*/*"),
        ("content-type", "application/json"),
        ("x-request-id", "abcdef-123456"),
        ("content-length", "0"),
        ("cache-control", "no-cache"),
        ("accept-language", "zh-CN,zh;q=0.9"),
    ] {
        v.push((k.to_string(), val.to_string()));
    }
    v
}

/// 解析器对比：同样一段真实请求头，rboot 的完整解析 vs httparse 的裸解析
fn bench_parse() {
    let head = "GET /json HTTP/1.1\r\nhost: bench\r\nconnection: keep-alive\r\nuser-agent: load/1.0\r\naccept: */*\r\naccept-encoding: gzip, deflate\r\nx-request-id: abcdef-123456\r\ncontent-length: 0\r\ncache-control: no-cache\r\n\r\n";
    let n = 300_000;

    let ours = bench("rboot-parse", n, || {
        match rsboot::server::parse::parse_head(head, 1 << 20) {
            Ok(p) => p.headers.len() + p.target.path.len(),
            Err(_) => 0,
        }
    });

    let mut headers = [httparse::EMPTY_HEADER; 16];
    let theirs = bench("httparse", n, || {
        let mut req = httparse::Request::new(&mut headers);
        match req.parse(head.as_bytes()) {
            Ok(_) => req.path.map(|p| p.len()).unwrap_or(0) + 10,
            Err(_) => 0,
        }
    });

    println!("解析同一段请求头（含方法、路径、8 个头、CL 解析）");
    println!("  rboot   {:>7.1} ns", ours);
    println!("  httparse{:>7.1} ns  （actix 用的就是它，裸解析、不做合法性校验）", theirs);
    println!("  差      {:>7.1} ns\n", ours - theirs);
}

fn main() {
    let data = Item {
        id: 42,
        name: "rsboot",
        tags: vec!["fast", "rust", "coroutine"],
    };
    let n = 200_000;

    let a = bench("old", n, || old_path(&data, Some("trace-abc-123")).len());
    let b = bench("new", n, || new_path(&data, Some("trace-abc-123")).len());

    // 无 trace_id 的版本单独量：少一个字段不等于同一路径
    let a2 = bench("old-no-trace", n, || old_path(&data, None).len());
    let b2 = bench("new-no-trace", n, || new_path(&data, None).len());

    println!("每响应耗时（含 1 次分配 + 序列化 + 拷贝）");
    println!("  with trace   old {:>7.1} ns  new {:>7.1} ns  省 {:>6.1} ns ({:.0}%)",
        a, b, a - b, (a - b) / a * 100.0);
    println!("  without trace old {:>7.1} ns  new {:>7.1} ns  省 {:>6.1} ns ({:.0}%)",
        a2, b2, a2 - b2, (a2 - b2) / a2 * 100.0);
    bench_parse();

    // 校准：这台 VM 上一次 Instant::now() 多贵。分阶段插桩每段要调两次
    let clk = bench("clock", 500_000, || {
        let a = Instant::now();
        let b = Instant::now();
        std::hint::black_box(a);
        b.elapsed().as_nanos() as usize
    });
    println!("一次 Instant::now() ≈ {:.1} ns（一段插桩 = 2 次，约 {:.0} ns 固定开销）\n", clk/2.0, clk);

    // 头部查找：一轮 8 次 lookup
    let hs = headers();
    let g_old = bench("get-old", 200_000, || {
        let mut hit = 0usize;
        for k in LOOKUPS {
            hit += old_get(&hs, k).map(|s| s.len()).unwrap_or(0);
        }
        hit
    });
    let g_new = bench("get-new", 200_000, || {
        let mut hit = 0usize;
        for k in LOOKUPS {
            hit += new_get(&hs, k).map(|s| s.len()).unwrap_or(0);
        }
        hit
    });
    println!("每请求 8 次头部查找");
    println!("  old {:>7.1} ns / 8 次  new {:>7.1} ns / 8 次  省 {:>6.1} ns ({:.0}%)",
        g_old, g_new, g_old - g_new, (g_old - g_new) / g_old * 100.0);

    println!("\n校验字节一致: {}", {
        let x = old_path(&data, Some("t"));
        let y = new_path(&data, Some("t"));
        x == y
    });
}

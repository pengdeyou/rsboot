<p align="center">
  <img src="docs/rsboot-logo.svg" alt="rsboot" width="200">
</p>

# rsboot

> ### The fastest way to *build* a Rust web service.
>
> Fastest to write, fastest to ship — and ahead of axum and actix-web on I/O-bound load — **9 687 QPS** at 200 concurrent clients / 20 ms delay, **96× its own synchronous predecessor**. One dependency line in `Cargo.toml`; signal handling, OpenAPI docs and connection pooling are built in — a few lines from an empty directory to a running endpoint.
>
> We are **not** the raw-throughput champion, and this README does not pretend otherwise: on the no-I/O JSON path actix-web leads by ~18 % at high concurrency. The measured table below shows every column, including the ones where we lose.

A Rust-ecosystem-native application framework — FastAPI-style declarative routing, typed extraction, automatic OpenAPI. **Synchronous handlers run on stackful coroutines over non-blocking sockets**: slow I/O yields the worker instead of parking it. The HTTP/1.1 stack, the coroutine scheduler and the event loop are written from scratch — no tokio, no hyper, no axum.

[English](README.md) · [中文](README.zh-CN.md) · [Deutsch](README.de.md) · [Français](README.fr.md) · [العربية](README.ar.md) · [Русский](README.ru.md)

**Status: alpha (`0.1.0-alpha.1`).** The core chain — HTTP, routing, extraction, response envelope, error handling, OpenAPI, module lifecycle, graceful shutdown, coroutine scheduling, database sockets — is implemented and covered by tests. SSE / WebSocket / JSON-RPC are not built yet; see [Known limitations](#known-limitations).

`rsboot` is the application framework. [rsbatis](https://github.com/pengdeyou/rsbatis) is the ORM. Separate crates, separate repositories, independent versions.

## Why it exists

FastAPI's appeal comes down to three things: declarative routes, types as validation, documentation generated for free. The mainstream Rust frameworks each miss one of them:

| | Handler | Trade-off |
|---|---|---|
| axum / actix-web | `async fn` | `Send + 'static` bounds; can't borrow `&Request` across an await point; async DB/HTTP pulls tokio + hyper + sqlx into the dependency tree |
| Synchronous frameworks | `fn` | Worker count equals core count, so throughput is capped at *workers ÷ blocking time* — one slow dependency stalls the whole server |

The second row is measured, not theoretical: with a 20 ms downstream delay and 200 concurrent clients, a purely synchronous implementation served **99 QPS**. Both workers were simply blocked.

rsboot splits the problem in two: **the connection layer is event-driven, the request layer runs on stackful coroutines.** Handlers keep ordinary synchronous signatures, slow I/O yields the worker instead of occupying it.

## Performance

Benchmarked against axum and actix-web. Setup: 4-core Linux VM, **2 worker threads** for all three servers, identical routes, all three measured round-robin inside the same time window, QPS reported.

| Scenario | rsboot | axum | actix-web |
|---|---:|---:|---:|
| `/slow` — 20 ms delay, 5 concurrent | **248** | 236 | 236 |
| `/slow` — 20 ms delay, 20 concurrent | **990** | 944 | 944 |
| `/slow` — 20 ms delay, 200 concurrent | **9 687** | 9 237 | 9 292 |
| `/json` — 4 concurrent, no blocking I/O | **160 006** | 130 504 | 140 917 |
| `/json` — 20 concurrent, no blocking I/O | **198 557** | 153 618 | 194 150 |

The two `/json` rows were re-measured on 2026-11-26; the `/slow` rows come from an earlier window on the same machine (the coroutine path has not changed since, but treat them as a separate run rather than a single snapshot).

What the table says, including the parts that don't flatter us:

- On blocking-ish workloads rsboot is the fastest of the three, but only by a few percent over axum/actix — the three converge because all of them keep workers busy. The interesting number is the gap against **our own synchronous version**: 99 → 9 687 QPS at 200 concurrency, **96×**. That is the whole reason the coroutine layer exists.
- Pure JSON throughput: rsboot leads at both concurrency levels in the current window — 13.5 % over actix-web and 22.6 % over axum at 4 clients; 2.3 % over actix and 29.2 % over axum at 20 clients. The 20-client lead is **inside run-to-run noise** (three rounds spanned 193 k–210 k on this 4-core VM), so read that row as "on par with actix", not as a win.
- Two figures this README used to print are **retired**. `157 730`: the load generator itself cost microseconds per request (two `Instant::now()` calls at ~1.8 µs each on this test VM), which flattened all three servers into the same number — the rows above come from a rebuilt client (`rdtsc` timing, no per-request allocation, no per-request syscall). `131 350`: measured before the envelope was rewritten (916 ns → 102 ns), before responses were batched into a single `writev`, and while every request still re-allocated its trace id.
- **One worker per core is the wrong default.** Every worker is its own event-loop thread, and the accept thread needs a core too, so `workers = cores` puts N+1 runnable threads on N cores. On this 4-core VM, 1 000 keep-alive connections: `WORKERS=1` 123 099, **2 → 196 569 / 190 044 / 191 828**, 3 → 177 476, 4 → 148 748, 6 → 138 460 QPS. The default is now `cores - 1` (floor 1), which is **+25 to +30 % over the previous default** with no code change on the hot path — it was pure oversubscription. If you benchmark with the client on the same machine, subtract one more core for it (`WORKERS=2` on a 4-core box); if the server owns the box, `cores - 1` is the safe setting.

- **Multi-connection throughput** (new client: one thread, non-blocking `poll`, N keep-alive connections, one request in flight per connection — this is the pattern a real gateway produces, and it is the first bench where the comparison is not dominated by the load generator): 1 000 connections, `/json`, rsboot 196 k / axum 218 k / actix 214 k; 200 connections, rsboot 207 k / axum 207 k. So we are **~10–13 % behind**, not ahead. The remaining gap is not CPU: per worker, `user` time is 2.4 µs/request here versus 2.6 µs for axum — essentially the same userland work — while **`sys` time is 9.8 µs vs 2.8 µs**. rsboot spends 3.5× more in the kernel, and its QPS is flat from 200 to 1 000 connections (139 k at both, under the old 4-worker default), which means it is a fixed per-request cost, not `poll`'s O(n) scan. Next step is an `epoll` backend on Linux (macOS keeps the `poll` path), which removes the per-round rescan of all fds; that is a structural change, not a tuning knob, so it is not in this commit.

- Idle keep-alive connections no longer hold 8 KiB each. `BufReader::new` reserves its default buffer eagerly, so 1 000 parked connections measured 15.6 MB RSS; with a 1 KiB start buffer plus `shrink_to` on idle (only after a full `io_timeout` of silence), the same 1 000 connections settle at 6.7 MB — **9.2 KB less per idle connection**, with no throughput change in the same bench window. Body reads are unaffected: `read_exact` bypasses the buffer once the remaining length exceeds capacity.

- Header parsing walks the request head **once**: one byte scan cuts the lines, verifies CR/LF (stray `\r`/`\n` is a 400 — that is the CRLF-injection entry point), remembers the colon, and OWS bounds. Token validation is a 256-entry table lookup, `Headers` is filled in one allocation, and `Connection` is compared token-by-token while parsing instead of being re-scanned and lower-cased afterwards. Micro-benchmark: **346 ns → 205 ns** for a 3-header request, **1 260 ns → 680 ns** for a realistic 12-header one. End to end with 3 headers that is 140 ns out of ~11 µs, i.e. inside the noise; it pays off on browser/gateway traffic, not on our own bench client.

- Route dispatch is a **segment trie**, not a scan. Routes sharing a prefix used to land in one bucket (`/api/v1/...`, 200 of them), so every request walked the bucket: 8 947 ns for the last static route, 15 576 ns for the last parameterised one. Dispatching now costs O(segments) — 218 ns / 291 ns for the same 200-route worst case, and 247 ns for a plain 4-route table — with three per-request allocations removed along the way. Micro-benchmarks only; end to end this is invisible at 4–8 routes, which is exactly why the scan never showed up in our own numbers.

- Response head buffers are now allocated **per connection instead of per request** (`fill_head` micro-benchmark: 163 ns → 109 ns per response). End to end that disappears into noise — an A/B over the same window gave median 230 200 vs 232 270 QPS with a ±25 % spread — so it is kept for the steady-state allocation cost, not for a headline number. For the record: `poll` blocking is 14 % of worker time and small responses already leave in one `writev`, so neither is where the remaining gap sits.
- One **fd per connection**, not two. `Conn` used to keep a `TcpStream` *and* a `try_clone` of it for the `BufReader`: one `dup(2)` per connection, two fds, two `close` calls. The write path now takes the fd straight out of the reader. Measured on the same window, accept-thread CPU per connection went **6.6 µs → 6.2 µs** (−4.6 % on that thread) and QPS moved 55.4 k → 56.6 k over three alternating rounds (+1.4 %, at the edge of noise). Kept for the syscall and the fd quota, not for the headline.

- Where the rest of the time actually goes. With `RSBOOT_PROFILE=1` the per-request split on this VM is: read 1.5–2.4 µs, parse ~0.4 µs, build ~0.4 µs, handle ~0.8 µs, **write 14–18 µs** — one `write` system call is ~80 % of every request. The same 60-byte write on a quiet loopback pair costs **3.1 µs**; under 20-client load it costs 14–17 µs, and the difference is the kernel waking a client thread on the same four cores, not anything this server does. The counters say `wr 1.00/req, eagain 0.00`: the write path issues exactly one syscall per response and never yields on back-pressure. So `/json` sits on its I/O floor — further work on the handler, parser or router moves single-digit percent of a 17 µs request.

- Downstream service calls: 40 concurrent clients × 20 ms dependency, cooperative mode **655 QPS / p99 62 ms** vs blocking baseline **33 QPS / p99 2 323 ms** — a 20× difference from a single flag (`RSBOOT_BLOCKING_IO`).

These numbers drift with the machine. Compare servers within one run, never across machines or against someone else's blog post.

## Design highlights

1. **No third-party runtime.** HTTP/1.1 parsing, keep-alive connection handling, `writev` batching, the poll loop and the M:N scheduler are all in-tree. The dependency list is serde-level, not tokio-level. Fewer moving parts, no version-lock to an async runtime's internals.

2. **Handlers stay synchronous, yet don't block the server.** A handler is `fn(&mut Request, &Context) -> Result<Response, ApiError>`. Stackful coroutines mean the state lives on an explicit stack, so `&mut Request` can be held across a yield point — something a compiler-generated state machine cannot express without lifetime gymnastics.

3. **Two layers, deliberately separated.** Connections are multiplexed by the event loop; requests run as coroutines. Coroutine count tracks *in-flight requests*, not open connections, so keep-alive connections cost a few bytes each instead of a 64 KB stack.

4. **Database sockets yield.** `mysql+co://…` routes the MySQL driver through coroutine-aware sockets, so a slow query parks the request instead of pinning a worker. The hook is process-wide but the mode is thread-local, so two engines in one process can't contaminate each other.

5. **Downstream calls yield too.** `ctx.service("name")` gives you a pool, retry and circuit breaker; the socket read is coroutine-aware, so one slow dependency degrades one endpoint instead of the whole server.

6. **One response envelope, one error type.** `Api(...)` wraps success and failure into the same shape; `ApiError` is the only error a handler returns, and it owns the status mapping. SQL text and internal paths go to the log, never to the client.

7. **Security fails at startup, not at runtime.** A malformed CORS config refuses to boot rather than silently allowing everything. Token-bucket rate limiting, security headers and connection-string redaction are built in.

8. **Lifecycle and graceful shutdown are first-class.** Modules implement `init / start / stop / drain` and shut down in reverse order. On SIGTERM the server enters a drain period (`http.drain_secs`) where `/ready` returns 503 so the load balancer can shed traffic *before* connections are severed; new requests during closing get 503 instead of a reset.

9. **OpenAPI is gone through, not bolted on.** `RouteDoc` sits on the route definition; `/openapi.json` and `/docs` are generated from the same source of truth as the handlers.

10. **Layered config, dev/prod split, hot reload.** YAML / ini / env layers, `app.env` switches behaviour, and the file watcher reloads without a restart.

## Quick start

```bash
cargo run --example hello
curl -i http://127.0.0.1:8000/api/hello/rsboot
```

Minimal shape:

```rust
use rsboot::{ApiError, App, Config, FromRequest, Path, Response, Router};

fn main() -> rsboot::Result<()> {
    let config = Config::new()
        .set("app.env", "dev")
        .set("http.bind", "127.0.0.1:8000")
        .set("http.docs", "true");

    let mut app = App::builder()
        .config(config)
        .database("default", "sqlite:///hello.db")
        .build()?;

    let router = Router::new()
        .get("/", |_, _| Response::text("rsboot is running\n"))?
        .get("/api/hello/{name}", |req, _| -> Result<String, ApiError> {
            let Path(name) = Path::<String>::from_request(req)?;
            Ok(format!("hello, {name}"))
        })?;

    app.serve(router)
}
```

`serve` installs signal handling for you: stop accepting, let workers finish, close modules in reverse order. No extra wiring on top.

## Known limitations

Stated plainly, because a framework that hides its gaps is worse than one that lists them:

- SSE, WebSocket and JSON-RPC 2.0 are not implemented yet (planned as S11–S13)
- gRPC is explicitly out of scope: it needs HTTP/2, which means pulling in `h2` and giving up the zero-runtime-dependency claim
- Authentication stops at `Bearer` / `ApiKey` extractors — there is no full role/permission system
- Coroutine stacks are a fixed 64 KB, so 10 k concurrent requests cost roughly 640 MB of stack memory
- Client-side **connect** is still blocking; only reads/writes after connect yield
- Reading request headers and bodies on the main path can still block a worker

## License

MIT OR Apache-2.0, at your option.

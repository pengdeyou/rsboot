<p align="center">
  <img src="docs/rsboot-logo.svg" alt="rsboot" width="200">
</p>

# rsboot

> ### The fastest way to *build* a Rust web service.
>
> Fastest to write, fastest to ship — and ahead of axum and actix-web on I/O-bound load — **9 687 QPS** at 200 concurrent clients / 20 ms delay, **96× its own synchronous predecessor**. One dependency line in `Cargo.toml`; signal handling, OpenAPI docs and connection pooling are built in — a few lines from an empty directory to a running endpoint.
>
> We are **not** the raw-throughput champion, and this README does not pretend otherwise: on the no-I/O JSON path actix-web leads by **~3 %** when both run four workers and the client pipelines 16 deep. The measured tables below show every column, including the ones where we lose.

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

Benchmarked against axum and actix-web. All servers run inside the same time window, round-robin, identical routes, QPS reported. Numbers from different windows are **not** comparable — the protocol changed, and the older rows are kept only because they are still the only measurement of that workload.

### How the current numbers are measured

- 4-core Linux box, **4 worker threads on every server**, client on **4 threads**, 1 000 keep-alive connections, `pipeline 16`.
- Each configuration runs **10 rounds × 8 s**, A/B alternating round by round; then a second batch with the order reversed. The 20 rounds are pooled and reported as a mean plus a win/loss split.
- Run-to-run noise on this box is **±0.35 %** with a 10:10 to 13:7 split. Anything smaller is not a result, and the two batches must point the same way.
- Current standing against actix-web 4 (same window, both at 4 workers): rsboot **999 411** vs actix **1 033 174** — **−3.3 %**, 1:19.

The full log — every change, the delta it produced, and everything that was measured and reverted — is in [docs/perf-log.md](docs/perf-log.md).

The `/slow` and `/json` rows below come from an **earlier window** (4-core VM, 2 worker threads everywhere, shorter runs).

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
- **One worker per core is the wrong default.** Every worker is its own event-loop thread, and the accept thread needs a core too, so `workers = cores` puts N+1 runnable threads on N cores. On this 4-core VM, 1 000 keep-alive connections: `WORKERS=1` 123 099, **2 → 196 569 / 190 044 / 191 828**, 3 → 177 476, 4 → 148 748, 6 → 138 460 QPS. The default is now `cores - 1` (floor 1), which is **+25 to +30 % over the previous default** with no code change on the hot path — it was pure oversubscription. If you benchmark with the client on the same machine, subtract one more core for it (`WORKERS=2` on a 4-core box).

  **Later correction (2026-11): the default is now `cores`.** The row above was measured with a 2-thread client sharing the box — that client is itself the bottleneck, and it also eats cores that the workers are being denied. Re-run with the client on 4 threads (still same machine, 1 000 connections, `/json`): `pipeline 16` W3 1 535 k → **W4 1 722 k (+12 %)**, `pipeline 1` W3 265 k → **W4 339 k (+28 %)**, and the run-to-run spread collapses from ±10 % to ±2 %. The accept thread only works while connections are being opened; at steady state it wakes once every 5 ms and costs nothing, so "N+1 runnable threads on N cores" was never the problem. (Scan above was measured on the `poll` path; with `epoll` landed the same box gives 202 k at `WORKERS=2` and 172 k at `WORKERS=3`.)

- **Multi-connection throughput** (new client: one thread, non-blocking `poll`, N keep-alive connections, one request in flight per connection — the pattern a real gateway produces, and the first bench where the load generator stops flattening all three servers): 1 000 connections, `/json`, **rsboot 201 865 / axum 201 624 / actix 212 016**; 200 connections, rsboot 195 652 / axum 221 382 / actix 201 002. At 1 000 connections we are level with axum and 5 % behind actix; at 200 we are still 11 % behind axum. What used to hurt was not userland — per request `user` is 2.4 µs here against axum's 2.6 µs — but the kernel: **9.8 µs `sys` per request against 2.8 µs**, and the QPS was flat from 200 to 1 000 connections (139 k at both under the old 4-worker default), so it was a fixed per-request cost rather than `poll`'s O(n) scan.

- **A second client thread changes the ranking.** Every number above came from a single-threaded client, which saturates at ~200 k QPS itself: at 1 000 connections all three servers were pinned against that ceiling, so "level with axum" was partly the client's limit, not ours. Giving the client two threads (500 connections each) lifts the ceiling: rsboot **221 428 / 219 306**, axum **199 690 / 199 307**, actix **263 162 / 262 465**. So actix is **20 % ahead**, and axum is behind us — the opposite order from the single-thread table. Per request we sit at 6.5 µs against actix's 5.2 µs; `user` is 2.7 µs against 1.9 µs, `sys` is the same (5.9 s per worker over the same window), so actix wins in userland, not in the kernel. The same window explains why tuning knobs stopped paying: `write` is 4.0 µs of the 6.3 µs request, `read` 1.1 µs, `parse`+`build` 0.46 µs, `handle` 0.5 µs. Two syscalls per request is already the minimum; closing the rest needs batching (io_uring or sending several responses in one syscall), which is a rewrite rather than a knob.

- **The worker default was the whole 20 %.** The table above had `WORKERS=2` set by hand, to leave cores for the client — that is the wrong setting. Sweeping workers with the 2-thread client at 1 000 connections, 15 s each: **W=1 123 827 / 125 035, W=2 227 418 / 220 739, W=3 269 438 / 266 476**. Three workers, which is what the library picks by itself on a 4-core box (`cores − 1`), beat two by **19 %**. The reason is that this load is latency-bound, not batch-bound: one request in flight per connection, each request ~7 µs, and dividing 1 000 connections over W workers shrinks both the bitmap and the ready-fd set per round, so the RTT of each request drops. More workers win until the box is oversubscribed, and 3 workers + 1 accept = 4 threads on 4 cores is exactly full.

- **HTTP/1.1 pipelining was silently dropping requests — and now batches the answers.** `BufReader` slurps several requests into its own buffer while reading the first one. That buffer is invisible to `poll`, so after answering request #1 the connection was parked waiting for an event that never arrives: a client sending 4 requests on one connection got **1 response**, and the other 3 sat until timeout. `serve_once` now loops while `reader.buffer()` is non-empty. Then the follow-up: queued responses are accumulated in `out_buf` and leave in **one** `write` (flush at 16 KiB; bodies over 8 KiB still go straight to `writev` so a download stays zero-copy, and a lone request keeps the old direct path byte for byte). Two-thread client, 1 000 connections, `/json`, 15 s:

  | pipeline | before | after |
  |---:|---:|---:|
  | 1 | 258 048 / 257 781 | 266 574 / 253 006 |
  | 4 | 374 021 (3 of 4 dropped) | **636 159 / 632 847** |
  | 16 | 516 710 (15 of 16 dropped) | **896 926 / 878 163** |

  Same window: actix **1 497 566** (p16) / 765 326 (p4), axum 198 732 (p16 — hyper does not pipeline). So actix is still ~40 % ahead when clients pipeline, but we went from *wrong answers* to 887 k.

- **The trace id is now 12 % of a pipelined request.** Batching amortises the syscalls away, so per-request cost fell from 7.3 µs to **2.55 µs** (read 0.8–1.4, parse 0.25, build 0.24, handle 0.43–1.04, write 0.23). With `RSBOOT_NO_TRACE=2`, 16-deep pipelining goes from 884 k to **1 028 k**: generating the id and inserting it twice costs **0.3 µs/request**. It is the two `Headers` inserts and two `Arc` clones, not the hex formatting — that is the next thing worth attacking.

- **The trace id no longer allocates, and no longer touches `Headers`.** Profile said the whole thing cost **198 ns/request** (the `build` stage sat at 238 ns versus 40 ns with `RSBOOT_NO_TRACE=2`), and it was three allocations plus one `Headers` insert (`String` for the hex, `Arc<str>` on top of it, then `insert_shared`). Now the id is formatted straight into a stack buffer (`write_trace_id(&mut [u8])`) and appended to the response head by `fill_head` — zero allocation, zero inserts. `build` dropped 238 → **139 ns**, and 16-deep pipelining went from 899 k to **972 137 / 970 340** (+8 %). `pipeline 1` is unchanged at 255 094 / 258 968.

- **The right worker count depends on the request shape, not on the core count.** Sweeping workers under 16-deep pipelining (2-thread client, 1 000 connections, `/json`): W=2 **874 k**, W=3 **886 k**, W=4 **933 k**, W=6 **1 203 k**, W=8 **1 122 k**, W=12 **1 155 k**, W=16 **1 133 k**. Under one-request-in-flight (`pipeline 1`): W=3 **256 k**, W=6 **241 k**, W=8 **223 k**. So under a saturating 2-thread client `cores − 1` looked right for browsers and plain gateways, while ~**1.5 × cores** won by 36 % when clients pipeline — with three workers a pipelining client spends too much of each round waiting, and six wedges fill the cores instead. At W=6 the poll blocking ratio is 9–10 %, i.e. the box is saturated: **1 188 338** is this 4-core VM's ceiling for us, and the profile there reads 4.3–5.4 µs per request (contention inflates each request, throughput still wins on parallelism). Recommendation: the default is now one worker per core (see the correction above); lower it explicitly only when the load generator shares the box.

- **Read buffer size: no effect under pipelining.** 1 KiB vs 8 KiB, measured 1 000 conns both ways: pipeline 16 **952 k vs 956 k**, pipeline 1 **260 k vs 260 k**. The 1 KiB buffer holds ~12 request heads, so 16-deep pipelining needs two reads — but a read amortised over 16 requests is ~60 ns, so it never mattered. Exposed as `RSBOOT_READ_BUF` for diagnosis; it is not a win.

- **The clock was frozen on this VM — and every number above was measured while busy-polling.** `rdtsc` passes the 20 ms calibration (2 596 ticks/µs) and then stops dead: 20 million back-to-back reads return the identical value. Consequences were silent, not loud: `now_ns()` became a constant, so `wait_fd_until` deadlines never arrived (read timeouts, idle-connection reaping and coroutine deadlines all stopped firing), and the worker's `poll` timeout clamped to 0 — every worker was spinning instead of sleeping. Fixed by detecting the freeze at runtime (64 consecutive identical *nanosecond* values, not raw ticks — a frozen TSC still jitters a few ticks) and falling back to `Instant`; `now_ns()` on the request path now refreshes only every 64th call and reads a thread-local cache in between, because `Instant` costs 1.8 µs here. Result: `pipeline 1` went from **256 k to 298 k** (+16 %), and `pipeline 16` went from 1 188 k to **1 045 k** — it *dropped*, because the old figure was the busy-poll stealing CPU from the client. Same window, three frameworks: p1 rsboot **298 327** / actix 276 240 / axum 248 641; p16 actix **1 556 135** / rsboot **1 045 360** / axum 200 116. So rsboot now leads actix by 8 % on the shape real gateways use, and trails by 33 % when clients pipeline.

- **Where the rest of a pipelined request goes: the coroutine scheduler, not any stage.** The `[prep]` line now has a `serve` stage, so a full round is accounted for: of a 6.9 ms round at 165 connections, **pump 99.7 %**, serve 0.17 %, poll wait 0.1 %. Per request the five stages total **1.0 µs** (read 115, parse 225, build 86, handle 346, write 226) while the round arithmetic says **2.8 µs** — the missing **1.8 µs** is inside `pump` itself: spawn, `serve_once` boundaries, `flush_out`, and dropping `Request`/`Response`/`Conn`. That is the next place to instrument, and the only place left that is big enough to matter.

- **A response body is worth 8 % under pipelining.** `/empty` (`Response::new(200)`, no body, no JSON) versus `/json` (60 B through `Response::json_with`): **1 126 078 vs 1 045 360**. The earlier A/B said "noise" — that was a contaminated window; in a clean one the single `to_vec()` per request is real. p1 is dominated elsewhere: `write` is 4.8 µs of 6.8 µs, and `flush` alone is 4.5 µs of pure syscall time, which no user-space change can remove.

- **`pump` was never the hole; the wake was.** `[prep]` said pump held 99.7 % of a round, but that is a container: it resumes coroutines and their service time is counted inside it. Net scheduler cost is **5.7 ns/connection**. What did cost money was `Shared::push` calling `waker.notify()` unconditionally — a `write(eventfd)` per request — while `spawn` and deadline wakeups run *on the worker's own thread*, where nobody is asleep. Split into `push_quiet` (local, no wake) and `push` (cross-thread, wakes): **999 k → 1 063 k, +6.5 %**, +15 % in the cleanest round.
- **A request head was copied twice.** `BufReader::read_until` filled a `line: Vec<u8>`, which was then pushed into `head: String` — every byte walked twice, with a `clear` + `from_utf8` per line. `read_head` now reads one whole buffer, scans for `\r\n\r\n` on it, and moves the segment with a single `extend`; the only extra work is a 1..3 byte overlap check at the buffer boundary so a terminator split across two fills is not missed. **1 031 k → 1 080 k, +4.7 %** over three alternating rounds, with `Chunked` tests that force `step = 1..3` so every split of `\r\n\r\n` is exercised.

- **The request line and target are scanned once, not three times.** `head.find("\r\n")` ran a general two-byte search for a separator that sits ~30 bytes in; `request_line.split(' ')` built an iterator and pulled four items just to confirm there was no third space; `target.split_once('?')` then `path.contains('#')` walked the target twice. All three are now single manual passes over the bytes: one to locate CRLF, one that records both space positions (a third space is still a 400), and one that finds `?` and `#` together. Microbenchmark: lean 3-header request **167 → 139 ns**, typical 12-header request **641 → 536 ns** (−17 % / −16 %).

  End to end this is worth about **1 %**: the `parse` stage moved from 225 to 214 ns while pipelined QPS stayed inside the noise band (1 045 360 → 1 053 033). Parsing is 8 % of the 2.8 µs a pipelined request actually costs, so 30 ns buys almost nothing. What remains in the parse path is three allocations per request — `Target { path, query }` both `String`, the `Arc<str>` that copies the whole head, and the `Headers` item array — and that is where httparse pulls ahead: it allocates nothing, returning byte spans into a buffer the caller owns. Matching it means turning `path`/`query` into spans and giving `Headers` a lifetime tied to the connection buffer, which changes the public shape of `Request`. Not worth 2–3 % when the 1.8 µs hole inside `pump` is twenty times larger.

- **The request path allocates nothing now: the head buffer is borrowed, not copied.** `Headers` gained a lifetime (`Headers<'a> { raw: Option<Cow<'a, str>> }`), and `Target`, `ParsedRequest` and `Request` all became `<'a>` carrying `&'a str` path/query straight out of the connection's head buffer. The old path did three allocations per request — `Arc::from(head)` copying the whole head, two `String`s for path and query, plus the `Headers` item array — and the first two are now gone. `Response` uses `Headers<'static>`: `Headers::new()` is defined on `'static`, so no caller had to write a lifetime, and `bench/rsboot_serve` compiles unchanged.

  Microbenchmark: lean 3-header **139 → 100 ns** (−28 %), typical 12-header **536 → 477 ns** (−11 %). That is past httparse's ~120 ns on a request this small, because we no longer copy the head at all. End to end, same window, three alternating rounds at 1 000 connections / p16 / W=3: old **932 697 / 933 437 / 945 068** (mean 937 k) versus new **969 841 / 950 505 / 941 955** (mean 954 k) — **+1.8 %**, with the `parse` stage at 181 ns (was 214). The limit is unchanged: parsing is now only 6 % of the 2.8 µs a pipelined request costs, so even a free parser would not close the gap to actix.

  Worth knowing for anyone building a `Request` by hand: it infers `'static` from string literals, so tests and examples keep working; only code that wants to borrow a buffer longer than a statement needs to name the lifetime.


  Behaviour change worth knowing: the id the server generates now appears **only in the response headers**. `req.request_id()` still returns whatever the client sent, but there is no longer a copy of the generated id in `req.headers`. If a handler logged `req.request_id()` to correlate with the response, it now gets `None` for requests that did not carry one.



- **Same window, all three servers at their own defaults:** 1 000 connections rsboot **268 163 / 269 438**, actix **260 051 / 261 986**, axum **217 062 / 196 272** — rsboot **2.6 % ahead of actix**, 23 % ahead of axum. At 200 connections: rsboot 269 566 / 271 213, actix 275 465 / 275 556, axum 200 650 / 203 006 — **1.9 % behind actix**. So the earlier "actix is 20 % ahead" was an artefact of hand-setting the worker count, not a property of either server. Parity at 200, a small lead at 1 000.


- **A reused fd was silently dropping epoll events.** Regression introduced with the `epoll` backend, caught by a short-connection A/B. `close(2)` removes the fd from the epoll set behind our back, while the bitmap still said "registered" — so the next connection that got the same fd number *never* received a read event and sat waiting for the 500 ms fallback. Idle keep-alive never notices; short connections do: 20 threads with `--close` went from P99 1.1 ms / max 2.3 ms to **P99 24 ms / max 167 s**. `Conn` now has a `Drop` that queues the fd (thread-local, lock-free) and the next round issues `EPOLL_CTL_DEL` before anything else; the periodic rebuild stays as a second line of defence. After the fix, same window: P99 **1.16 ms**, max 2.9 ms.

- **Response headers are pre-sized.** `Headers::new()` starts with room for 4 entries, and the parsed request table reserves one extra slot for `x-request-id`. Both used to grow from zero — 2–3 `realloc`s per request on the hottest path. Effect: **+2.5 %** (221 k → 226 k on the 2-thread client), and the cost of injecting the trace id fell from 6.3 % to 3.7 %. That split was measured with `RSBOOT_NO_TRACE=1` (fixed id, still inserted) and `=2` (skipped): the id itself is nearly free — it was the two header inserts.

- **The write counter was measuring two workers at once.** `WRITE_CALLS` was a process-wide `AtomicU64`, but the request count it gets divided by is thread-local — so each worker's line divided *both workers' writes* by *its own* sample count. That is where the old `wr 1.80/req` and `wr 0.32/req` came from: one real total split twice, not a second syscall. Made thread-local, every worker now prints **wr 1.00/req, eagain 0.00** — exactly one `write` per response, never a back-pressure yield. Split on the 2-thread client (1 000 connections, `WORKERS=2`): read 1.2–1.3 µs, parse 0.27, build 0.21–0.23, handle 0.55, write 4.9–5.7 µs, total 7.2–8.0 µs under load.

- **Smaller socket buffers: no effect.** The default `SO_SNDBUF` auto-tunes and can grow to megabytes; a 260-byte response doesn't need that queue. `RSBOOT_SNDBUF` / `RSBOOT_RCVBUF` (opt-in, Linux only, unset = previous behaviour exactly) pin both to 4 KiB. Three alternating rounds: default 228 179 / 223 861 / 221 611 vs 4 KiB 225 005 / 223 228 / 225 172 — **−0.04 %, inside the noise**. The knob stays as a diagnostic switch; it is not a win, and it is recorded here so nobody re-runs the same experiment expecting one.


- Idle keep-alive connections no longer hold 8 KiB each. `BufReader::new` reserves its default buffer eagerly, so 1 000 parked connections measured 15.6 MB RSS; with a 1 KiB start buffer plus `shrink_to` on idle (only after a full `io_timeout` of silence), the same 1 000 connections settle at 6.7 MB — **9.2 KB less per idle connection**, with no throughput change in the same bench window. Body reads are unaffected: `read_exact` bypasses the buffer once the remaining length exceeds capacity.

- Header parsing walks the request head **once**: one byte scan cuts the lines, verifies CR/LF (stray `\r`/`\n` is a 400 — that is the CRLF-injection entry point), remembers the colon, and OWS bounds. Token validation is a 256-entry table lookup, `Headers` is filled in one allocation, and `Connection` is compared token-by-token while parsing instead of being re-scanned and lower-cased afterwards. Micro-benchmark: **346 ns → 205 ns** for a 3-header request, **1 260 ns → 680 ns** for a realistic 12-header one. End to end with 3 headers that is 140 ns out of ~11 µs, i.e. inside the noise; it pays off on browser/gateway traffic, not on our own bench client.

- Route dispatch is a **segment trie**, not a scan. Routes sharing a prefix used to land in one bucket (`/api/v1/...`, 200 of them), so every request walked the bucket: 8 947 ns for the last static route, 15 576 ns for the last parameterised one. Dispatching now costs O(segments) — 218 ns / 291 ns for the same 200-route worst case, and 247 ns for a plain 4-route table — with three per-request allocations removed along the way. Micro-benchmarks only; end to end this is invisible at 4–8 routes, which is exactly why the scan never showed up in our own numbers.

- Response head buffers are now allocated **per connection instead of per request** (`fill_head` micro-benchmark: 163 ns → 109 ns per response). End to end that disappears into noise — an A/B over the same window gave median 230 200 vs 232 270 QPS with a ±25 % spread — so it is kept for the steady-state allocation cost, not for a headline number. For the record: `poll` blocking is 14 % of worker time and small responses already leave in one `writev`, so neither is where the remaining gap sits.
- One **fd per connection**, not two. `Conn` used to keep a `TcpStream` *and* a `try_clone` of it for the `BufReader`: one `dup(2)` per connection, two fds, two `close` calls. The write path now takes the fd straight out of the reader. Measured on the same window, accept-thread CPU per connection went **6.6 µs → 6.2 µs** (−4.6 % on that thread) and QPS moved 55.4 k → 56.6 k over three alternating rounds (+1.4 %, at the edge of noise). Kept for the syscall and the fd quota, not for the headline.

- Where the rest of the time actually goes. With `--features prof` plus `RSBOOT_PROFILE=1` the per-request split on this VM is: read 1.5–2.4 µs, parse ~0.4 µs, build ~0.4 µs, handle ~0.8 µs, **write 14–18 µs** — one `write` system call is ~80 % of every request. The same 60-byte write on a quiet loopback pair costs **3.1 µs**; under 20-client load it costs 14–17 µs, and the difference is the kernel waking a client thread on the same four cores, not anything this server does. The counters say `wr 1.00/req, eagain 0.00`: the write path issues exactly one syscall per response and never yields on back-pressure. So `/json` sits on its I/O floor — further work on the handler, parser or router moves single-digit percent of a 17 µs request.

- Downstream service calls: 40 concurrent clients × 20 ms dependency, cooperative mode **655 QPS / p99 62 ms** vs blocking baseline **33 QPS / p99 2 323 ms** — a 20× difference from a single flag (`RSBOOT_BLOCKING_IO`).

These numbers drift with the machine. Compare servers within one run, never across machines or against someone else's blog post.

### Already tried, measured, and rejected

Kept here so nobody re-runs the same experiment expecting a win. Protocol as above; a negative delta means the change was **slower**.

| Change | Result | Why it did not pay |
|---|---|---|
| Merge the name-field passes (colon + trailing OWS + token check into one) | −0.06 %, 13:7 | Neutral. Parser cost is not "how many times we walk the bytes" |
| `Hdr` 64 → 32 bytes (`Item{pos,len,tag}` + side table) | −0.59 %, 9:11, batches disagreed | One more indirection on every header access |
| `get_unchecked` in the parse hot loops | **−1.69 %**, 14:6 | Bounds checks were already free; `unsafe` blocked other optimisations |
| Hand-rolled CRLF scan in the response head | −1.36 %, 12:8 | `str::contains` goes through vectorised memchr |
| `from_utf8_unchecked` on the header slice | −0.99 %, 12:8 | Same — the check is not where the time is |
| Thread-local pool for response header `Vec`s | −0.35 %, 12:8 | And `Drop` on `Headers` breaks the borrow in `serve_once` |
| `-C target-cpu=x86-64-v3` / `native` | +0.9 %, but AB −0.4 % / BA +2.3 % | Two batches pointing opposite ways = noise |
| Inline 12 header slots to save one `malloc` | −8.3 % | memset of 12 × 64 bytes costs more than the allocation |
| Batch every response into `out_buf` (drop the direct-write path) | −4.3 % | A pipelining client keeps requests in flight; the server rarely sees more than one |
| `SO_REUSEPORT`, per-worker accept | no change | The accept thread wakes once every 5 ms at steady state |
| Smaller `SO_SNDBUF` / `SO_RCVBUF` | −0.04 % | Inside the noise |
| 4 KiB read buffer as the default | +0.8 %, not significant | 1 KiB already holds ~12 request heads |
| Bundled tcache allocator (feature `tc-alloc`) | +0.36 % | Ceiling is ~0.5 %; off by default |

The correlating profile, for anyone who wants to attack the remaining 3 %: two syscalls are 70 % of a request (3.5–4 µs of 5 µs), userland is ~1 µs (`parse` 275 ns, `build` 145, `handle` 555). So the gap is ~150 ns and can only live in those three stages — but three structural attempts at `parse` have all landed inside the noise, which says the 275 ns is fixed per-request work (item construction, `Vec` bookkeeping, span pushes, closing validation), not scan width. The next honest step is to instrument `handle` itself at 1/64 sampling and split its 555 ns into routing, context, the user handler and response building, rather than guess again.

## Uniform responses

`Res` gives the shapes a JSON API actually needs — one line each, no builder chain, no `Ok(...)`:

| Call | Returns | Status | Body |
|---|---|---|---|
| `Res::success("ok")` | `Response` | 200 | `{"code":200,"message":"ok"}` |
| `Res::data(payload)` | `Response` | 200 | `{"code":200,"message":"success","data":<payload>}` |
| `Res::fail("out of stock")` | `Response` | 400 | `{"code":400,"message":"out of stock"}` |
| `Res::fail_with(Status::CONFLICT, "...")` | `Response` | as given | same shape, `code` is the status |
| `Res::err("...")` | `ApiError` | 400 | same shape, for `Err(...)` / `?` |
| `Res::err_with(status, "...")` | `ApiError` | as given | same as above |

The low-ceremony form is a handler that returns `Response` — neither branch needs `Ok`:

```rust
fn ping(_: &mut Request, _: &Context) -> Response {
    Res::success("ok")
}

fn detail(_: &mut Request, _: &Context) -> Response {
    if miss() {
        return Res::fail("no such record");   // 400 + the same shape
    }
    Res::data(row)
}
```

Use a `Result` signature plus `Res::err` only when you need `?` propagation or you are reusing calls that already return `ApiError`:

```rust
fn detail(_: &mut Request, _: &Context) -> Result<Response, ApiError> {
    let row = load_row()?;                        // ? works as usual
    if row.is_empty() {
        return Err(Res::err("no such record"));   // ApiError, fits Err
    }
    Ok(Res::data(row))
}
```

Four things worth knowing:

- `code` is the **numeric HTTP status** (200 / 400 / 404 / 500), not a second string code space. The status already carries the meaning; inventing another code just gives you two codes to reconcile.
- No `data` field when there is no data — not `data: null`. `{"code":200,"message":"ok"}` is enough, and clients branch on "is `data` present" instead of "is `data` null".
- `fail` and `err` are two names because a Rust function has exactly one return type: `fail` returns `Response` (can be returned directly), `err` returns `ApiError` (fits `Err(...)`). Same payload, matched to two handler signatures.
- For all of these the wrapper is **unconditional** — it does not depend on `http.envelope`. A shape you asked for explicitly should not be reshaped by a global switch.
- `data` serialises into the same buffer (no `serde_json::Value` tree in between), and the field order is byte-identical to what `http.envelope` produces, so turning that switch on does not change a single byte of these responses.

## Error codes

`code` is a 4-digit number: **the first 3 digits are the HTTP status, the last is the sub-code**. Gateways retry/cache/alert on the status; services branch on the sub-code. One table serves both, so you never maintain two code spaces and translate between them.

| Code | Status | Meaning |
|---|---|---|
| `200` | 200 | success |
| `4000` | 400 | bad request (generic) |
| `4001`–`4009` | 400 | your business codes (`4001` out of stock, `4002` insufficient balance, …) |
| `4010` | 401 | unauthenticated |
| `4030` | 403 | forbidden |
| `4040` | 404 | not found |
| `4090` | 409 | conflict |
| `4220` | 422 | validation failed |
| `4290` | 429 | rate limited |
| `5000` | 500 | service error (generic) |
| `5001` | 500 | upstream / third-party service error |
| `5002` | 500 | database error |
| `5003` | 500 | timeout |
| `5010` | 501 | method not implemented |
| `5050` | 505 | unsupported HTTP version |

`status_of` only reads the leading three digits, so longer codes still work: `5001` → 500, `4220` → 422, `200` → 200. The constants live in `rsboot::web::code`.

```rust
// Business errors: allocate your own 4xx codes
Res::fail(4002, "insufficient balance");   // 400 {"code":4002,"message":"insufficient balance"}

// Service errors: the 5xx segment
Res::fail(5001, "upstream unavailable");   // 500 {"code":5001,"message":"upstream unavailable"}
Err(Res::err(5002, "inventory service down"))
ApiError::upstream("third-party timeout")  // same as code = 5001
```

**Early rejections in the protocol layer use the same shape.** 400 (malformed), 413 (body too large), 414 (request line too long), 431 (headers too large), 501 (unknown method) and 505 (unsupported version) are refused by the parser, but the body is still `{"code":<n>,"message":"..."}` with `connection: close` — clients do not need a second parser for errors that never reached a handler.

## Extension crates

The core carries only four things: the protocol, the router, extractors and errors.
Capabilities that only some apps need live in separate crates of the same workspace,
so you never pay for what you do not use:

| crate | what it gives you | extra deps |
| --- | --- | --- |
| `rsboot-ops` | `/healthz`, `/readyz` (per-database detail), `/metrics` (Prometheus text) | `serde_json` |
| `rsboot-cookie` | cookie reading, `Set-Cookie` (`SameSite` / `HttpOnly` / `Secure` / `Max-Age`) | none |
| `rsboot-form` | `Form<T>` for `application/x-www-form-urlencoded` | `serde_urlencoded` |
| `rsboot-static` | static files: ETag / 304 / single-range 206 / traversal guard / index | none |
| `rsboot-view` | Smarty-style templates: `{$x}`, `{if}`, `{foreach}`, `{include}`, pipe modifiers | `serde_json` |

```rust,ignore
use rsboot_ops as ops;
use rsboot_cookie::{self as cookie, SetCookie, SameSite};
use rsboot_form::Form;

ops::Metrics::install();          // opt-in: without it the hot path pays nothing

let r = Router::new()
    .get("/healthz", |_, _| ops::healthz())?
    .get("/readyz", |_, ctx: &Context| ops::readyz(ctx))?
    .get("/metrics", |_, _| ops::Metrics::scrape())?;
```

The core exposes just two generic hooks for them: `Headers::append` (a response may
carry several `Set-Cookie` lines) and `web::stats` (a `fn(u16, u64)` observer that
costs one atomic load when nothing is installed).

### Static files

`Static` keeps the root behind an `Arc<OnceLock<Option<PathBuf>>>`, so cloning it is
one refcount bump and you can hang it straight on a route:

```rust,ignore
use rsboot_static::Static;

let assets = Static::dir("public").max_age(3600).immutable();
let files  = Static::dir("uploads").index("index.html");

let r = Router::new()
    .get("/assets/*path", {
        let s = assets.clone();
        move |req: &mut Request, _| s.serve_or_404(req)
    })?
    .fallback(files.clone().fallback());
```

Notes: `If-None-Match` wins over `If-Modified-Since` (`*` counts as a match); multi-range
`Range` is ignored and served as 200 (assembling the parts costs more than it saves);
every path segment is `percent_decode`d and rejected if it is `.`, `..`, contains a
separator or NUL, and after `canonicalize` the result must still `starts_with(root)`,
so a symlink pointing outside is refused too. A miss returns **404** rather than `None`
-- "refused" and "not mine" are different answers, and conflating them misleads the layer
above.

### Templates

`rsboot-view` borrows its architecture from minijinja (no token stream, parse while
reading, compile once and render many) and its syntax from Smarty. Not pulling in
`minijinja` or `tera` is simple: it would break the zero-dependency promise, and either
one speaks Jinja (`{{ }}` / `{% %}`), which is not `{$x}`.

```rust,ignore
use std::sync::Arc;
use rsboot_view::Engine;

// compiled once at startup, shared across workers via Arc; rendering only does lookups
let view = Arc::new(Engine::new().add_dir("templates")?);

let r = Router::new().get("/orders", {
    let v = view.clone();
    move |req: &mut Request, _| {
        let data = serde_json::json!({ "title": "Orders", "items": list });
        match v.render("orders.tpl", &data) {
            Ok(html) => Response::html(html),
            Err(e) => e.to_api_error().into_response(),
        }
    }
})?;
```

| syntax | meaning |
| --- | --- |
| `{$name}` `{$user.name}` `{$list[0]}` | read a value, dots and indexes mix |
| `{if $a > 1}...{elseif $b}...{else}...{/if}` | conditionals, `and` / `or` / `not` |
| `{foreach $list as $it}` `as $k => $v` `{foreachelse}` | iterate an array or object |
| `{$x\|escape}` `\|raw` `\|upper` `\|lower` `\|trim` `\|default:"-"` `\|length` `\|truncate:20` `\|json` `\|join:", "` `\|nl2br` | modifiers, chainable |
| `{include "x.tpl"}` / `{include file="x.tpl"}` | pull in a sub-template sharing the parent scope |
| `{literal}...{/literal}` `{* ... *}` | verbatim output / comment |
| `{myfunc a=$x}` | call a function registered with `Engine::func` |

Two intentional differences from Smarty: **auto-escaping is on by default** (Smarty
leaves it off), because forgetting `|escape` costs you an XSS while forgetting `|raw`
only shows a literal `&lt;div&gt;`; and `|json` turns `< > & '` into `\uXXXX`, so
embedding it in `<script>` or a `data-*` attribute cannot close the tag early. `include`
is capped at 32 levels, so a cyclic include is a clear error instead of a blown worker
stack.

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

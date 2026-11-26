# benches

Horizontal comparison of rsboot, axum and actix-web on the same endpoints, the same worker count and the same client. Every number in the top-level README comes from this directory.

```
benches/
├── load/          the client: raw sockets, std only, neutral toward all three servers
├── rsboot_serve/  rsboot server (path dependency on ../..)
├── axum_serve/    axum server
├── actix_serve/   actix-web server
├── slow_down/     a downstream service that answers after 20 ms (used by /down)
├── micro/         in-process micro-benchmarks: envelope, request-head parsing, header lookup
├── run.sh         builds nothing, runs all three round-robin and writes results.md
└── results.md     last recorded run on a 4-core Linux VM, 2 workers
```

## Reading the numbers

Three constraints apply to every figure here:

1. On a 4-core box the servers and the client compete for the same cores, so absolute QPS is a *same-machine relative* measure only.
2. All servers run **2 workers**, regardless of core count. The client runs 2 / 20 / 50 threads, which approximates concurrent connections.
3. The client is neutral: it speaks the HTTP/1.1 wire format itself, so no framework's client library can skew the comparison.

## Why the client had to be rebuilt

The first version of `load` called `Instant::now()` twice per request. On the test VM one such call costs **~1.8 µs** (the vDSO path is not in effect there), so the client alone was burning 3.6 µs per request and every framework converged to the same number — 248 / 236 / 236 at 5 concurrent clients is not the servers converging, it is the client dominating.

The rebuilt client:

- reads through one `BufReader` allocated per connection, not per request;
- keeps the request bytes, the request line and the body buffer across requests;
- parses the status line with a hand-written decimal parser and prefix matching instead of `format!`;
- takes time with `rdtsc`, calibrated once against a 30 ms sleep.

Nothing in the per-request path allocates or calls into the kernel beyond the socket itself. Only then do the differences between the servers show up.

## Running it

```bash
# full comparison: builds all three, runs each round-robin, writes results.md
./run.sh

# one server, one endpoint, 20 client threads, 3 seconds
cd rsboot_serve && cargo build --release
WORKERS=2 BIND=127.0.0.1:9001 ./target/release/rsboot-serve &
./load/target/release/load 127.0.0.1:9001 /json 20 3

# micro-benchmarks (no sockets): envelope, parse_head vs httparse, header lookup
cd micro && cargo run -q --release --bin micro
```

The `/down` route needs `DOWN=http://127.0.0.1:9004` in the environment and `slow_down` running:

```bash
cd slow_down && cargo build --release && BIND=127.0.0.1:9004 ./target/release/slow-down &
```

## Profiling a server

`RSBOOT_PROFILE=1` turns on the five-stage sampler (1 of every 64 requests, plus a per-worker count of how long `poll` blocks). Timing *every* request costs about half the throughput, which is why the sample exists at all.

```bash
RSBOOT_PROFILE=1 WORKERS=2 BIND=127.0.0.1:9001 ./target/release/rsboot-serve
# [profile] 5000 req | read 5573  parse 2407  build 2134  handle 2953  write 12076  = 25143 ns
# [poll] 114000 次 | 平均等待 27197 ns | 平均本轮 194975 ns | 阻塞占 13.9%
```

`RSBOOT_NO_CORO=1` runs the same server without coroutines, to separate scheduler cost from HTTP cost.

## Known results (4-core Linux VM, 2 workers)

| Scenario | rsboot | axum | actix-web |
|---|---:|---:|---:|
| `/slow` 20 ms, 5 concurrent | 248 | 236 | 236 |
| `/slow` 20 ms, 20 concurrent | 990 | 944 | 944 |
| `/slow` 20 ms, 200 concurrent | 9 687 | 9 237 | 9 292 |
| `/json` 4 concurrent | 112 267 | 92 280 | 110 919 |
| `/json` 20 concurrent | 131 350 | 133 843 | 155 315 |

Micro-benchmarks, same machine: envelope without a trace id 102 ns (was 916 ns); `parse_head` on a realistic 8-header request 757 ns against `httparse`'s 122 ns.

Two things worth stating plainly:

- We lead on the I/O-bound path and at low concurrency, and we trail actix-web by ~18 % on pure JSON throughput at 20 concurrent clients.
- `poll` blocking accounts for only ~14 % of worker time, and small responses already leave in a single `write` call — so neither the event loop nor the syscall count is what separates us from actix-web. The gap is in the header parser and the per-request bookkeeping.

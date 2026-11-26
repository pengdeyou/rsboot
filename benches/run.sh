#!/usr/bin/env bash
# 单机横向对比：rsboot / axum / actix-web，同端点、同 worker 数、同压测器。
#
# 三条必须一起读的约束：
# 1. 4 核机器上服务端与压测器抢同一批核，绝对值只是"同机相对比较"；
# 2. 服务端 worker 一律 2，压测线程取 2/20/50 三档（≈ 并发连接数）；
# 3. 压测器是自写的 raw socket 客户端，只依赖 std，对三者完全中立。
set -u
B="$(cd "$(dirname "$0")" && pwd)"
L="$B/load/target/release/load"
med() { sort -n | sed -n 2p; }

case_run() { # $1=框架 $2=端口 $3=路径 $4=线程 $5=额外参数
  local fw=$1 port=$2 path=$3 th=$4 extra=${5:-}
  local qps=() p99=() mx=() err=()
  for _ in 1 2 3; do
    out="$("$L" "127.0.0.1:$port" "$path" "$th" 5 $extra)"
    qps+=("$(echo "$out" | awk '/^QPS/{print $2}')")
    p99+=("$(echo "$out" | awk '/^延迟/{print $9}')")
    mx+=("$(echo "$out" | awk '/^延迟/{print $NF}')")
    err+=("$(echo "$out" | awk '/^错误/{print $2}')")
  done
  printf "| %-8s | %-6s | %-14s | %8s | %7s | %7s | %5s |\n" "$fw" "$th" "$path${extra:+ $extra}" \
    "$(printf '%s\n' "${qps[@]}" | med)" "$(printf '%s\n' "${p99[@]}" | med)" \
    "$(printf '%s\n' "${mx[@]}" | med)" "$(printf '%s\n' "${err[@]:-0}" | sed -n 1p)"
}

wait_port() { for _ in $(seq 1 40); do (exec 3<>/dev/tcp/127.0.0.1/"$1") 2>/dev/null && return 0; sleep 0.1; done; return 1; }

{
echo "| 框架 | 压测线程 | 场景 | QPS | P99(ms) | max(ms) | 错误 |"
echo "|---|---|---|---|---|---|---|"
echo "[rsboot]" >&2
(cd "$B" && WORKERS=2 BIND=127.0.0.1:9001 ./rsboot_serve/target/release/rsboot-serve >/tmp/b_rs.log 2>&1 &)
wait_port 9001
for th in 2 20 50; do case_run rsboot 9001 /json $th; done
case_run rsboot 9001 /json 20 --close
case_run rsboot 9001 /items/42 20
pkill -x rsboot-serve; sleep 1.5

echo "[axum]" >&2
(cd "$B/axum_serve" && BIND=127.0.0.1:9002 ./target/release/axum-serve >/tmp/b_ax.log 2>&1 &)
wait_port 9002
for th in 2 20 50; do case_run axum 9002 /json $th; done
case_run axum 9002 /json 20 --close
case_run axum 9002 /items/42 20
pkill -x axum-serve; sleep 1

echo "[actix]" >&2
(cd "$B/actix_serve" && BIND=127.0.0.1:9003 ./target/release/actix-serve >/tmp/b_ac.log 2>&1 &)
wait_port 9003
for th in 2 20 50; do case_run actix 9003 /json $th; done
case_run actix 9003 /json 20 --close
case_run actix 9003 /items/42 20
pkill -x actix-serve; sleep 1
} > "$B/results.md" 2>/tmp/bench_progress.log
cat "$B/results.md"

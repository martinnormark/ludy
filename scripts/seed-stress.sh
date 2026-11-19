#!/bin/sh
# Make a drive with thousands of synthetic files, for scroll measurements.
#   scripts/seed-stress.sh [count] [server-url]
# The files take no disk. Then: cargo run -p ludy-app -- --open Stress --perf
set -eu
N="${1:-5000}"
B="${2:-${LUDY_SERVER:-http://127.0.0.1:9000}}"
curl -fsS -o /dev/null -X PUT "$B/Stress" || true
# No simulated latency while seeding; put the previous link back afterwards.
LINK=$(curl -fsS "$B/_stats" | sed -n 's/.*"latency_ms":\([0-9]*\).*/\1/p')
MBPS=$(curl -fsS "$B/_stats" | sed -n 's/.*"mbps":\([0-9]*\).*/\1/p')
curl -fsS -o /dev/null -X PUT "$B/_config?latency_ms=0&mbps=0"
i=1
while [ "$i" -le "$N" ]; do
  case $(( i % 6 )) in 0) e=mov;; 1) e=mp4;; 2) e=wav;; 3) e=png;; 4) e=braw;; *) e=txt;; esac
  curl -fsS -o /dev/null -X PUT -H "x-ludy-synthetic-size: $(( i * 7919 % 900000000 + 1000 ))" -H "x-ludy-synthetic-seed: $i" \
    "$B/Stress/$(printf 'clip_%05d' "$i").$e" &
  [ $(( i % 16 )) -eq 0 ] && wait
  i=$(( i + 1 ))
done
wait
curl -fsS -o /dev/null -X PUT "$B/_config?latency_ms=${LINK:-30}&mbps=${MBPS:-400}"
echo "Stress: $N synthetic files"

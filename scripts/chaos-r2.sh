#!/bin/sh
# Chaos, in a loop, with the server's blobs in a real Cloudflare R2 bucket.
#
#   scripts/chaos-r2.sh <work-dir> [rounds]
#
# Each round runs, against `ludy-rivet` beside a Rivet engine of the rig's
# own, with the bucket reached through the rig's store proxy:
#
#   store    the five scenarios that take the store away on purpose: gone,
#            silent, silent for longer than a request lives, connections
#            cut in mid-flight, gone under the collector
#   server   the server's soak: random commits while the server is killed
#            and frozen, messages are lost, and the store has weather
#   engine   the engine's soak: the same under the real engine, with the
#            app killed as well
#   mount    (first round, and every fifth) scripts/chaos-mount.sh on a
#            Compose project with docker-compose.r2.yml: the image, built
#            from this checkout, a real mount, both containers killed, the
#            mounting process killed
#   suites   (first round only) every scenario of both suites, to see that
#            what passes on a folder passes on a bucket
#
# What `kill -9` takes rotates with the round: the server and the engine,
# the server alone, the engine alone. Seeds come from the clock and are
# written down. A round's output, the server's logs and a line per leg in
# <work-dir>/rounds.md are kept. The loop stops at the first leg that
# fails and leaves everything where it is.
#
# After every leg the drives that leg made are removed from the bucket, by
# id, from the server logs the rig kept: nothing else in the bucket is
# touched. If the loop is interrupted in the middle of a leg, that leg's
# drives are still in the bucket; `scripts/s3-drives.py clean <work-dir>`
# removes them, from the logs under the round's folder. The bucket is the one in .env (R2_ACCOUNT_ID, R2_BUCKET,
# R2_ACCESS_KEY_ID, R2_SECRET_ACCESS_KEY); use one made for this.
#
# Needs Docker running, and builds what it runs. To leave it running:
#   nohup scripts/chaos-r2.sh /tmp/ludy-chaos-r2 6 > /tmp/ludy-chaos-r2.out 2>&1 &
# and read <work-dir>/status and <work-dir>/rounds.md.
#
# Settings: CHAOS_R2_STEPS (steps of each soak, 300), CHAOS_R2_MOUNT=0 to
# leave the mount leg out, CHAOS_R2_SUITES=0 to leave the full suites out.
set -eu
WORK="${1:?usage: chaos-r2.sh <work-dir> [rounds]}"
ROUNDS="${2:-3}"
STEPS="${CHAOS_R2_STEPS:-300}"
HERE="$(cd "$(dirname "$0")/.." && pwd)"
mkdir -p "$WORK"
WORK="$(cd "$WORK" && pwd)"
cd "$HERE"

say() { printf '%s  %s\n' "$(date +%H:%M:%S)" "$*"; }
status() { printf '%s  %s\n' "$(date '+%Y-%m-%d %H:%M:%S')" "$*" > "$WORK/status"; say "$*"; }
count() { scripts/s3-drives.py count; }

docker version > /dev/null 2>&1 || { echo "Docker is not running"; exit 2; }
status "building"
cargo build --release -p ludy-rivet -p ludy-cli 2>&1 | tail -1
cargo test -p ludy-core --no-run 2>&1 | tail -1

# The bucket, for the rig: these four go to the server the rig starts and
# nowhere else. Only this script sets them, so no other test run uses a bucket.
eval "$(scripts/s3-drives.py env)"
export LUDY_PROVIDER=rivet
say "the bucket before: $(count)"
[ -f "$WORK/rounds.md" ] || printf '| Round | Leg | kill -9 takes | Seed | Result | Wall time | Left in the bucket after cleaning |\n|---|---|---|---|---|---|---|\n' > "$WORK/rounds.md"

# leg <round-dir> <name> <kill> <seed> <command...>: run one leg, keep its
# output and the server's logs, clean its drives out of the bucket, write a
# line. Returns the leg's own result.
leg() {
  dir="$1"; name="$2"; kill="$3"; seed="$4"; shift 4
  mkdir -p "$dir/logs-$name"
  status "round $ROUND: $name (kill -9 takes $kill, seed $seed)"
  started=$(date +%s)
  result=passed
  # The rig's own folders go under the round too, so that a leg that is
  # interrupted leaves its server logs where `clean` can find them later.
  mkdir -p "$dir/tmp-$name"
  ( export LUDY_RIG_LOG_DIR="$dir/logs-$name" LUDY_RIVET_KILL="$kill" TMPDIR="$dir/tmp-$name"; "$@" ) > "$dir/$name.out" 2>&1 || result=FAILED
  took=$(( $(date +%s) - started ))
  cleaned="$(scripts/s3-drives.py clean "$dir/logs-$name" "$dir/tmp-$name" 2>&1 || true)"
  printf '| %s | %s | %s | %s | %s | %s s | %s |\n' "$ROUND" "$name" "$kill" "$seed" "$result" "$took" "$(count)" >> "$WORK/rounds.md"
  say "  $result in $took s; $cleaned"
  grep -E '^\| (Store|kill -9|Commits applied|Timeouts|Reads checked|Requests sent)' "$dir/$name.out" | sed 's/^/    /' || true
  [ "$result" = passed ]
}

soak() { cargo test -p ludy-core --test "$1" soak -- --ignored --test-threads=1 --nocapture; }
store_scenarios() { cargo test -p ludy-core --test worker_chaos store -- --ignored --test-threads=1 --nocapture; }
# Both, whatever the first one does: a failure in one must not hide the other's.
suites() {
  bad=0
  cargo test -p ludy-core --test worker_chaos -- --ignored --test-threads=1 --nocapture --skip soak || bad=1
  cargo test -p ludy-core --test sealed_engine -- --ignored --test-threads=1 --nocapture --skip soak || bad=1
  return $bad
}
# The mount script writes the id of the drive it made where `clean` finds it.
mount_leg() { CHAOS_MOUNT_BUILD=1 CHAOS_MOUNT_STORE=r2 LUDY_CHAOS_DRIVE_LOG="$1/logs-mount/drive.log" scripts/chaos-mount.sh "$1/mount" "${CHAOS_R2_MOUNT_FILES:-300}" "${CHAOS_R2_MOUNT_MIB:-50}" 9320; }

ROUND=1
while [ "$ROUND" -le "$ROUNDS" ]; do
  DIR="$WORK/round-$ROUND"
  mkdir -p "$DIR"
  case $(( ROUND % 3 )) in 1) KILL=both ;; 2) KILL=server ;; *) KILL=engine ;; esac
  SEED=$(( $(date +%s) % 100000 + ROUND ))
  fail() { status "round $ROUND FAILED in $1; everything is in $DIR"; exit 1; }

  leg "$DIR" store "$KILL" - store_scenarios || fail store
  export WORKER_CHAOS_SEED="$SEED" WORKER_CHAOS_STEPS="$STEPS" SEALED_SEED="$SEED" SEALED_STEPS="$STEPS"
  leg "$DIR" server "$KILL" "$SEED" soak worker_chaos || fail "the server's soak"
  leg "$DIR" engine "$KILL" "$SEED" soak sealed_engine || fail "the engine's soak"
  if [ "${CHAOS_R2_MOUNT:-1}" = 1 ] && [ $(( ROUND % 5 )) -eq 1 ]; then
    leg "$DIR" mount both - mount_leg "$DIR" || fail "chaos on a mount"
  fi
  if [ "${CHAOS_R2_SUITES:-1}" = 1 ] && [ "$ROUND" -eq 1 ]; then
    leg "$DIR" suites "$KILL" - suites || fail "the full suites"
  fi
  ROUND=$(( ROUND + 1 ))
done
status "done: $ROUNDS round(s) passed; the bucket now: $(count)"

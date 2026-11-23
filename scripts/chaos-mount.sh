#!/bin/sh
# A destructive run against a real mount and a real server, with the real
# binaries.
#
#   scripts/chaos-mount.sh <work-dir> [files] [copy-MiB] [port]
#
# 1. Start a server of its own (docker compose, in a project, on a port and
#    with volumes of its own), make a drive, mount it.
# 2. Flood a folder on the mounted drive with small files and start copying
#    a large one.
# 3. Take the server away in the middle: SIGKILL both containers, wait,
#    start them again on the same volumes.
# 4. Check from a second keystore, with a cache of its own, that the server
#    ends up with every file, byte for byte.
# 5. Kill the server again, write more files, kill -9 the mounting process
#    too (an app crash with unsent writes), bring both back, and check that
#    the unsent files arrive.
#
# The mount is hidden (nobrowse) and lives under <work-dir>, so the Finder
# and other apps leave it alone. Needs Docker running and a release build:
#   cargo build --release -p ludy-cli
# At the end the server's containers and volumes are removed.
set -eu
WORK="${1:?usage: chaos-mount.sh <work-dir> [files] [copy-MiB] [port]}"
FILES="${2:-2000}"
COPY_MIB="${3:-200}"
PORT="${4:-9310}"
HERE="$(cd "$(dirname "$0")/.." && pwd)"
LUDY="$HERE/target/release/ludy"
PROJECT="ludy-chaos-$PORT"
export LUDY_SERVER="http://127.0.0.1:$PORT" LUDY_PORT="$PORT"

[ -x "$LUDY" ] || { echo "build first: cargo build --release -p ludy-cli"; exit 2; }
mkdir -p "$WORK"
WORK="$(cd "$WORK" && pwd)"
MNT="$WORK/mnt/Chaos"
for d in home cache home2 cache2 src out; do rm -rf "${WORK:?}/$d"; done
mkdir -p "$WORK/mnt" "$WORK/src"

say() { printf '%s  %s\n' "$(date +%H:%M:%S)" "$*"; }
compose() { docker compose -p "$PROJECT" -f "$HERE/docker-compose.yml" "$@"; }
# The Mac that mounts, and a second one that only checks.
mac()   { LUDY_HOME="$WORK/home"  LUDY_CACHE_DIR="$WORK/cache"  LUDY_CACHE_MB=300 "$LUDY" "$@"; }
other() { LUDY_HOME="$WORK/home2" LUDY_CACHE_DIR="$WORK/cache2" LUDY_CACHE_MB=300 "$LUDY" "$@"; }
server_up() { compose up -d --wait >> "$WORK/server.log" 2>&1; }
server_kill() { compose kill -s SIGKILL >> "$WORK/server.log" 2>&1; }
# Started directly, not through `mac`: the pid kept has to be the process
# that serves the mount, not a shell around it.
start_mount() { LUDY_HOME="$WORK/home" LUDY_CACHE_DIR="$WORK/cache" LUDY_CACHE_MB=300 "$LUDY" mount Chaos --nobrowse --root "$WORK/mnt" >> "$WORK/mount.log" 2>&1 & echo $! > "$WORK/mount.pid"; }
mounted() { /sbin/mount | grep -q " on $MNT "; }
wait_mounted() { i=0; until mounted; do i=$((i+1)); [ $i -lt 150 ] || { echo "mount did not appear"; cat "$WORK/mount.log"; exit 1; }; sleep 0.2; done; }
wait_unmounted() { i=0; while mounted; do i=$((i+1)); [ $i -lt 1500 ] || { echo "mount did not go away"; exit 1; }; sleep 0.2; done; }
cleanup() { compose down -v >> "$WORK/server.log" 2>&1 || true; }
trap cleanup EXIT

# write <first> <count> <folder>: deterministic small files, through the mount.
write_files() {
python3 - "$MNT/$3" "$1" "$2" <<'PY'
import os, random, sys
folder, first, count = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
os.makedirs(folder, exist_ok=True)
for i in range(first, first + count):
    r = random.Random(i)
    with open(os.path.join(folder, "note_%05d.bin" % i), "wb") as f:
        f.write(r.randbytes(1000 + r.randrange(7000)))
PY
}

# compare <folder> <first> <count> [big-file]: exit 0 if what was downloaded
# is exactly what was written, 1 if some of it is not there yet, 2 if any of
# it is wrong.
compare() {
python3 - "$WORK/out" "$1" "$2" "$3" "${4:-}" "$WORK/src" <<'PY'
import hashlib, os, random, sys
out, folder, first, count, big, src = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4]), sys.argv[5], sys.argv[6]
missing = wrong = 0
for i in range(first, first + count):
    r = random.Random(i)
    want = r.randbytes(1000 + r.randrange(7000))
    try:
        with open(os.path.join(out, folder, "note_%05d.bin" % i), "rb") as f:
            got = f.read()
    except OSError:
        missing += 1
        continue
    if len(got) != len(want):
        missing += 1
    elif got != want:
        wrong += 1
there = os.path.join(out, folder)
extra = len(os.listdir(there)) - count if os.path.isdir(there) else 0
def digest(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()
if big:
    try:
        if os.path.getsize(os.path.join(out, big)) != os.path.getsize(os.path.join(src, big)):
            missing += 1
        elif digest(os.path.join(out, big)) != digest(os.path.join(src, big)):
            wrong += 1
            print("FAILED: %s differs from the source" % big)
    except OSError:
        missing += 1
if wrong or extra > 0:
    print("FAILED: %d wrong, %d files nobody wrote" % (wrong, max(extra, 0)))
    sys.exit(2)
if missing:
    print("still missing: %d" % missing)
    sys.exit(1)
print("%d small files%s on the server, all as written" % (count, " and " + big if big else ""))
PY
}

# verify <folder> <first> <count> [big-file]: download with the second
# keystore until everything is there, then it must be right.
verify() {
  deadline=$(( $(date +%s) + 600 ))
  while :; do
    rm -rf "${WORK:?}/out"; mkdir -p "$WORK/out"
    other get Chaos "$1" "$WORK/out/$1" > "$WORK/get.log" 2>&1 || true
    if [ -n "${4:-}" ]; then other get Chaos "$4" "$WORK/out/$4" >> "$WORK/get.log" 2>&1 || true; fi
    code=0
    compare "$@" > "$WORK/compare.log" || code=$?
    case "$code" in
      0) cat "$WORK/compare.log"; return 0 ;;
      1) [ "$(date +%s)" -lt "$deadline" ] || { echo "FAILED: not there after 10 minutes"; cat "$WORK/compare.log" "$WORK/get.log"; exit 1; }
         sleep 2 ;;
      *) cat "$WORK/compare.log"; exit 1 ;;
    esac
  done
}

say "start a server of its own on port $PORT, make a drive, mount it (hidden) at $MNT"
server_up
mac mkdrive Chaos > /dev/null
other add "$(mac key Chaos)" > /dev/null
start_mount; wait_mounted

say "flood: $FILES small files, and a $COPY_MIB MiB copy"
head -c $((COPY_MIB * 1048576)) /dev/urandom > "$WORK/src/big.bin"
write_files 0 "$FILES" flood &
FLOOD=$!
cp "$WORK/src/big.bin" "$MNT/big.bin" &
COPY=$!
sleep 4
say "SIGKILL the server's two containers while they are in flight"
server_kill
sleep 5
say "the server comes back on the same volumes"
server_up
wait $FLOOD; wait $COPY
say "writers finished; waiting for the server to hold everything"
verify flood 0 "$FILES" big.bin

say "the server goes down; $((FILES / 4)) more files are written; then the mounting process is killed too"
server_kill
write_files 100000 $((FILES / 4)) offline
sleep 1
kill -9 "$(cat "$WORK/mount.pid")"
# The kernel only notices a dead mount when something asks it for
# something: untouched, this one was still in the mount table three minutes
# later. One `ls`, left to time out by itself, starts the clock.
(ls "$MNT" > /dev/null 2>&1 &)
say "waiting for the kernel to drop the dead mount (about a minute)"
wait_unmounted
say "server and mount start again"
server_up
start_mount; wait_mounted
verify offline 100000 $((FILES / 4))
ls "$MNT/offline" | wc -l | xargs -I{} echo "$(date +%H:%M:%S)  the remounted drive lists {} files in offline/"

say "eject"
kill -INT "$(cat "$WORK/mount.pid")"
wait_unmounted
say "PASSED"

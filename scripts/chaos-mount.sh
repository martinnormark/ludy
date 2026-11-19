#!/bin/sh
# A destructive run against a real mount, with the real binaries.
#
#   scripts/chaos-mount.sh <work-dir> [files] [copy-MiB] [port]
#
# 1. Flood a folder on the mounted drive with small files and start copying
#    a large one.
# 2. "Deploy" the cloud in the middle: kill -9 the server, wait, start it
#    again on the same data.
# 3. Check that the cloud ends up with every file, byte for byte.
# 4. Stop the cloud, write more files, kill -9 the mounting process too
#    (an app crash with unsent writes), bring both back, and check that the
#    unsent files arrive.
#
# The mount is hidden (nobrowse) and lives under <work-dir>, so the Finder
# and other apps leave it alone. Needs release builds:
#   cargo build --release -p ludy-cloud -p ludy-cli
set -eu
WORK="${1:?usage: chaos-mount.sh <work-dir> [files] [copy-MiB] [port]}"
FILES="${2:-2000}"
COPY_MIB="${3:-200}"
PORT="${4:-9310}"
HERE="$(cd "$(dirname "$0")/.." && pwd)"
CLOUD="$HERE/target/release/ludy-cloud"
LUDY="$HERE/target/release/ludy"
B="http://127.0.0.1:$PORT"
MNT="$WORK/mnt/Chaos"
export LUDY_SERVER="$B" LUDY_HOME="$WORK/home" LUDY_CACHE_DIR="$WORK/cache" LUDY_CACHE_MB=300

[ -x "$CLOUD" ] && [ -x "$LUDY" ] || { echo "build first: cargo build --release -p ludy-cloud -p ludy-cli"; exit 2; }
mkdir -p "$WORK"
rm -rf "$WORK/data" "$WORK/home" "$WORK/cache" "$WORK/src"
mkdir -p "$WORK/mnt" "$WORK/src"

say() { printf '%s  %s\n' "$(date +%H:%M:%S)" "$*"; }
start_cloud() { "$CLOUD" --data "$WORK/data" --port "$PORT" --latency-ms 20 --mbps 400 --quiet >> "$WORK/cloud.log" 2>&1 & echo $! > "$WORK/cloud.pid"; }
cloud_up() { i=0; until curl -fsS -o /dev/null "$B/_stats" 2>/dev/null; do i=$((i+1)); [ $i -lt 100 ] || { echo "cloud did not start"; exit 1; }; sleep 0.1; done; }
start_mount() { "$LUDY" mount Chaos --nobrowse --root "$WORK/mnt" >> "$WORK/mount.log" 2>&1 & echo $! > "$WORK/mount.pid"; }
mounted() { /sbin/mount | grep -q " on $MNT "; }
wait_mounted() { i=0; until mounted; do i=$((i+1)); [ $i -lt 150 ] || { echo "mount did not appear"; cat "$WORK/mount.log"; exit 1; }; sleep 0.2; done; }
wait_unmounted() { i=0; while mounted; do i=$((i+1)); [ $i -lt 600 ] || { echo "mount did not go away"; exit 1; }; sleep 0.2; done; }

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

# verify <folder> <first> <count> [big-file]: wait until the cloud has exactly these, then compare bytes.
verify() {
python3 - "$B" "$1" "$2" "$3" "${4:-}" "$WORK/src" <<'PY'
import hashlib, json, random, sys, time, urllib.parse, urllib.request
base, folder, first, count, big, src = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4]), sys.argv[5], sys.argv[6]
want = {}
for i in range(first, first + count):
    r = random.Random(i)
    want["%s/note_%05d.bin" % (folder, i)] = r.randbytes(1000 + r.randrange(7000))
def listing():
    with urllib.request.urlopen(base + "/Chaos", timeout=30) as f:
        return {o["key"]: o for o in json.load(f)["objects"] if not o["key"].endswith("/")}
deadline = time.time() + 600
while True:
    try:
        have = listing()
        missing = [k for k, v in want.items() if k not in have or have[k]["size"] != len(v)]
        if big and (big not in have):
            missing.append(big)
        if not missing:
            break
    except Exception as e:
        missing = ["(cloud not answering: %s)" % e]
    if time.time() > deadline:
        print("FAILED: still missing after 10 minutes:", len(missing), missing[:5]); sys.exit(1)
    time.sleep(1)
bad = 0
for key, data in want.items():
    with urllib.request.urlopen(base + "/Chaos/" + urllib.parse.quote(key), timeout=30) as f:
        if f.read() != data:
            bad += 1
if big:
    h = hashlib.sha256()
    with urllib.request.urlopen(base + "/Chaos/" + urllib.parse.quote(big), timeout=300) as f:
        while True:
            chunk = f.read(1 << 20)
            if not chunk: break
            h.update(chunk)
    local = hashlib.sha256(open(src + "/" + big, "rb").read()).hexdigest()
    if h.hexdigest() != local:
        bad += 1
        print("FAILED: %s differs from the source" % big)
print("%d small files%s in the cloud, %d wrong" % (len(want), " and " + big if big else "", bad))
sys.exit(1 if bad else 0)
PY
}

say "start the cloud and mount a new drive (hidden) at $MNT"
start_cloud; cloud_up
curl -fsS -o /dev/null -X PUT "$B/Chaos"
start_mount; wait_mounted

say "flood: $FILES small files, and a $COPY_MIB MiB copy"
head -c $((COPY_MIB * 1048576)) /dev/urandom > "$WORK/src/big.bin"
write_files 0 "$FILES" flood &
FLOOD=$!
cp "$WORK/src/big.bin" "$MNT/big.bin" &
COPY=$!
sleep 4
say "deploy: kill -9 the cloud while they are in flight"
kill -9 "$(cat "$WORK/cloud.pid")"
sleep 5
say "the cloud comes back on the same data"
start_cloud; cloud_up
wait $FLOOD; wait $COPY
say "writers finished; waiting for the cloud to hold everything"
verify flood 0 "$FILES" big.bin

say "the cloud goes down; $((FILES / 4)) more files are written; then the mounting process is killed too"
kill -9 "$(cat "$WORK/cloud.pid")"
write_files 100000 $((FILES / 4)) offline
sleep 1
kill -9 "$(cat "$WORK/mount.pid")"
say "waiting for the kernel to drop the dead mount (about a minute)"
wait_unmounted
say "cloud and mount start again"
start_cloud; cloud_up
start_mount; wait_mounted
verify offline 100000 $((FILES / 4))
ls "$MNT/offline" | wc -l | xargs -I{} echo "$(date +%H:%M:%S)  the remounted drive lists {} files in offline/"

say "eject"
kill -INT "$(cat "$WORK/mount.pid")"
wait_unmounted
kill "$(cat "$WORK/cloud.pid")" 2>/dev/null || true
say "PASSED"

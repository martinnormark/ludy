#!/bin/sh
# Make a drive with thousands of small files, for scroll measurements.
#   scripts/seed-stress.sh [count]
# The files are real: each is cut, sealed and uploaded to the server at
# $LUDY_SERVER (default http://127.0.0.1:8080). 5,000 of them are about
# 5 MB in all. Then: cargo run -p ludy-app -- --open Stress/clips --perf
#
# Set LUDY to a built binary to skip cargo: LUDY=target/release/ludy
set -eu
N="${1:-5000}"
LUDY="${LUDY:-cargo run -q -p ludy-cli --}"
CLIPS="$(mktemp -d)"
trap 'rm -rf "${CLIPS:?}"' EXIT

$LUDY server
$LUDY drives | awk '{print $1}' | grep -qx Stress || $LUDY mkdrive Stress
python3 - "$CLIPS" "$N" <<'PY'
import os, random, sys
folder, count = sys.argv[1], int(sys.argv[2])
ext = ["mov", "mp4", "wav", "png", "braw", "txt"]
for i in range(1, count + 1):
    r = random.Random(i)
    with open(os.path.join(folder, "clip_%05d.%s" % (i, ext[i % 6])), "wb") as f:
        f.write(r.randbytes(200 + r.randrange(1800)))
PY
$LUDY put Stress "$CLIPS" clips
echo "Stress/clips: $N files. Open it with: cargo run -p ludy-app -- --open Stress/clips --perf"

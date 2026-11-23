#!/bin/sh
# Make three demo drives on a running server and fill them.
#   scripts/seed-demo.sh [path-to-a-video]
# The server is the one at $LUDY_SERVER (default http://127.0.0.1:8080, where
# `docker compose up -d --wait` answers). The drives' keys go into this Mac's
# keystore, so the app lists them afterwards.
#
# The video and the text files are uploaded for real: cut, sealed and stored.
# The large files (12 to 180 GB) are generated content: each is a sealed
# manifest that says "this many bytes of the stream for this seed", so they
# take no storage and no upload, and read like any other file.
#
# Set LUDY to a built binary to skip cargo: LUDY=target/release/ludy
set -eu
VIDEO="${1:-../e58b62c20765eacd37457c1a53d2e97ace458f85.mp4}"
LUDY="${LUDY:-cargo run -q -p ludy-cli --}"
TEXT="$(mktemp)"
trap 'rm -f "$TEXT"' EXIT

$LUDY server
have() { $LUDY drives | awk '{print $1}' | grep -qx "$1"; }
drive() { if have "$1"; then echo "$1 is already here"; else $LUDY mkdrive "$1"; fi; }
# A file a previous run already made is left as it is; any other failure stops the script.
synth() {
  if out=$($LUDY synth "$1" "$2" "$3" "$4" 2>&1); then echo "$out"
  elif echo "$out" | grep -q "is already there"; then echo "$1/$2 is already there"
  else echo "$out" >&2; exit 1
  fi
}
text()  { printf '%s\n' "$3" > "$TEXT"; $LUDY put "$1" "$TEXT" "$2"; }

drive Footage
drive Archive
drive Projects

if [ -f "$VIDEO" ]; then
  $LUDY put Footage "$VIDEO" demo.mp4
else
  echo "no video at $VIDEO; skipping the real file"
fi
synth Footage "raw/take-01.braw" 50GB 7
synth Footage "raw/take-02.braw" 24GB 8
text  Footage "shot-list.txt" "take 01: wide, take 02: close"

synth Archive "2025/drone-master.braw" 180GB 4
synth Archive "2025/interview-cam-a.mov" 12GB 3
synth Archive "2024/festival-cut.mov" 36GB 5

text Projects "spring-film/cut-v3.edl" "TITLE: spring film, cut 3"
text Projects "readme.txt" "Project files live here."

$LUDY drives

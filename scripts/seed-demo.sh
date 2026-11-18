#!/bin/sh
# Fill a running ludy-cloud with demo drives.
#   scripts/seed-demo.sh [path-to-a-video] [server-url]
# The video is uploaded for real; everything else is synthetic and takes no disk.
set -eu
VIDEO="${1:-../e58b62c20765eacd37457c1a53d2e97ace458f85.mp4}"
B="${2:-${LUDY_SERVER:-http://127.0.0.1:9000}}"

drive() { curl -fsS -o /dev/null -X PUT "$B/$1" || true; }
synth() { curl -fsS -o /dev/null -X PUT -H "x-ludy-synthetic-size: $3" -H "x-ludy-synthetic-seed: $4" "$B/$1/$2"; }
text()  { printf '%s\n' "$3" | curl -fsS -o /dev/null -T - "$B/$1/$2"; }

drive Footage
drive Archive
drive Projects

if [ -f "$VIDEO" ]; then
  echo "uploading $VIDEO to Footage/demo.mp4"
  curl -fsS -o /dev/null -T "$VIDEO" "$B/Footage/demo.mp4"
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

curl -fsS "$B/" | python3 -c 'import json,sys
for d in json.load(sys.stdin)["drives"]:
    print("%-10s %8.1f GB  %d objects" % (d["name"], d["bytes"]/1e9, d["objects"]))'

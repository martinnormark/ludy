#!/usr/bin/env python3
"""Look at, and clean up after, test drives in the bucket the R2 settings name.

    scripts/s3-drives.py env                    print what the test rig needs, as shell exports
    scripts/s3-drives.py count                  how many objects the bucket holds, and for how many drives
    scripts/s3-drives.py clean <log-dir>...     delete the objects of every drive named in the server
                                                logs under those folders, and nothing else

The settings are R2_ACCOUNT_ID, R2_BUCKET, R2_ACCESS_KEY_ID and
R2_SECRET_ACCESS_KEY, read from the file LUDY_R2_ENV names (default: .env
beside docker-compose.yml). They are handed to curl on its standard input,
so they never show in a process list, and nothing here prints them except
`env`, whose output is meant for `eval`.

`clean` deletes by drive id only: objects under `d/<id>/` for ids that
appear as `/drives/<id>` in a log the chaos rig kept. Whatever else is in
the bucket, another drive's blobs included, is left alone.
"""
import os
import re
import subprocess
import sys
import xml.etree.ElementTree as ET

HERE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
NEEDED = ["R2_ACCOUNT_ID", "R2_BUCKET", "R2_ACCESS_KEY_ID", "R2_SECRET_ACCESS_KEY"]
# The rig's probe for "do drives answer": never a real drive.
PROBE = "f" * 32


def settings():
    path = os.environ.get("LUDY_R2_ENV", os.path.join(HERE, ".env"))
    values = {}
    try:
        with open(path) as f:
            for line in f:
                line = line.strip()
                if line and not line.startswith("#") and "=" in line:
                    key, value = line.split("=", 1)
                    values[key.strip()] = value.strip().strip('"').strip("'")
    except OSError as e:
        sys.exit(f"cannot read {path}: {e}")
    missing = [k for k in NEEDED if not values.get(k)]
    if missing:
        sys.exit(f"{path} does not set {', '.join(missing)}")
    return values


def curl(config):
    """Run curl with its options, the credentials among them, on standard input."""
    return subprocess.run(["curl", "-K", "-"], input=config, capture_output=True, text=True)


def signed(v):
    return f'user = "{v["R2_ACCESS_KEY_ID"]}:{v["R2_SECRET_ACCESS_KEY"]}"\naws-sigv4 = "aws:amz:auto:s3"\nsilent\nshow-error\n'


def base(v):
    return f'https://{v["R2_ACCOUNT_ID"]}.r2.cloudflarestorage.com/{v["R2_BUCKET"]}'


def list_keys(v, prefix):
    keys, token = [], None
    while True:
        url = f"{base(v)}?list-type=2&prefix={prefix}" + (f"&continuation-token={token}" if token else "")
        out = curl(f'url = "{url}"\n{signed(v)}write-out = "\\n%{{http_code}}"\n')
        body, _, code = out.stdout.rpartition("\n")
        if code != "200":
            sys.exit(f"listing {prefix or 'the bucket'} failed: HTTP {code} {re.sub('<[^>]+>', ' ', body)[:200]} {out.stderr[:200]}")
        root = ET.fromstring(body)
        ns = root.tag.split("}")[0] + "}" if "}" in root.tag else ""
        for item in root.findall(f"{ns}Contents"):
            keys.append((item.find(f"{ns}Key").text, int(item.find(f"{ns}Size").text)))
        if root.findtext(f"{ns}IsTruncated") != "true":
            return keys
        token = root.findtext(f"{ns}NextContinuationToken")


def delete_keys(v, keys):
    """One DELETE per object, many at a time, in one curl."""
    failed = 0
    for start in range(0, len(keys), 500):
        batch = keys[start : start + 500]
        config = signed(v) + "parallel\nparallel-max = 32\n"
        for key in batch:
            config += f'next\n{signed(v)}request = "DELETE"\nurl = "{base(v)}/{key}"\nwrite-out = "%{{http_code}}\\n"\noutput = "/dev/null"\n'
        out = curl(config)
        codes = out.stdout.split()
        failed += sum(1 for c in codes if c not in ("204", "200")) + (len(batch) - len(codes))
    return failed


def drives_in_logs(folders):
    ids = set()
    for folder in folders:
        for root, _, files in os.walk(folder):
            for name in files:
                if name.endswith(".log"):
                    with open(os.path.join(root, name), errors="replace") as f:
                        ids.update(re.findall(r"/drives/([0-9a-f]{32})", f.read()))
    ids.discard(PROBE)
    return sorted(ids)


def main():
    command = sys.argv[1] if len(sys.argv) > 1 else ""
    v = settings()
    if command == "env":
        print(f'export LUDY_RIG_S3_BUCKET="{v["R2_BUCKET"]}"')
        print(f'export LUDY_RIG_S3_ENDPOINT="https://{v["R2_ACCOUNT_ID"]}.r2.cloudflarestorage.com"')
        print(f'export LUDY_RIG_S3_KEY_ID="{v["R2_ACCESS_KEY_ID"]}"')
        print(f'export LUDY_RIG_S3_SECRET="{v["R2_SECRET_ACCESS_KEY"]}"')
        print('export LUDY_RIG_S3_REGION="auto"')
    elif command == "count":
        keys = list_keys(v, "")
        drives = {k.split("/")[1] for k, _ in keys if k.startswith("d/") and k.count("/") >= 3}
        other = sum(1 for k, _ in keys if not k.startswith("d/"))
        print(f"{len(keys)} objects, {sum(s for _, s in keys)} bytes, {len(drives)} drives, {other} objects that are not a drive's")
    elif command == "clean" and len(sys.argv) > 2:
        ids = drives_in_logs(sys.argv[2:])
        deleted = failed = 0
        for drive in ids:
            keys = [k for k, _ in list_keys(v, f"d/{drive}/")]
            if keys:
                failed += delete_keys(v, keys)
                deleted += len(keys)
        print(f"{len(ids)} drives in the logs, {deleted} objects deleted" + (f", {failed} deletes failed" if failed else ""))
        sys.exit(1 if failed else 0)
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()

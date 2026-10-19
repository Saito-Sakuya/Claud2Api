#!/usr/bin/env python3
"""Package a compatible Linux AMD64 binary; does not publish anything."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tarfile

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--binary", required=True)
parser.add_argument("--tag", required=True)
parser.add_argument("--output", default="release")
args = parser.parse_args()
binary = Path(args.binary).resolve()
description = json.loads(subprocess.check_output([str(binary), "--describe-build"], timeout=5))
if args.tag != description["version"]:
    raise SystemExit("release tag must equal the built version (v + Cargo package version)")
header = binary.read_bytes()[:20]
if header[:6] != b"\x7fELF\x02\x01" or header[18:20] != b"\x3e\x00":
    raise SystemExit("release must be Linux AMD64 ELF")
size = binary.stat().st_size
if not 0 < size <= 128 << 20:
    raise SystemExit("release binary exceeds the updater limit (128 MiB)")
output = Path(args.output)
output.mkdir(parents=True, exist_ok=True)
name = "claud2api-linux-amd64"
shutil.copy2(binary, output / name)
digest = hashlib.sha256((output / name).read_bytes()).hexdigest()
manifest = dict(description, binary=name, size=size, sha256=digest)
(output / "claud2api-update.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
root = Path(__file__).resolve().parents[1]
license_name = "claud2api-licenses.tar.gz"
with tarfile.open(output / license_name, "w:gz") as archive:
    for filename in ("LICENSE", "THIRD_PARTY.md", "THIRD_PARTY_ZH.md", "LICENSES"):
        archive.add(root / filename, arcname=filename)

def entry(filename):
    text = (root / filename).read_text(encoding="utf-8")
    heading = "## " + args.tag + " "
    lines = text.splitlines()
    start = next((i for i, line in enumerate(lines) if line.startswith(heading)), None)
    if start is None:
        raise SystemExit("missing release notes for " + args.tag + " in " + filename)
    end = next((i for i in range(start + 1, len(lines)) if lines[i].startswith("## ")), len(lines))
    return "\n".join(lines[start + 1:end]).strip()

notes = "## 中文\n\n" + entry("CHANGELOG.md") + "\n\n## English\n\n" + entry("CHANGELOG_EN.md")
(output / "RELEASE_NOTES.md").write_text(notes + "\n", encoding="utf-8")
checksums = []
for filename in (name, "claud2api-update.json", license_name):
    checksums.append(hashlib.sha256((output / filename).read_bytes()).hexdigest() + "  " + filename)
(output / "SHA256SUMS").write_text("\n".join(checksums) + "\n", encoding="utf-8")
print(json.dumps(manifest))

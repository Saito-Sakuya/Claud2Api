"""Collect the actual locked Cargo license files. Run after cargo metadata.

Usage: python3 scripts/license_inventory.py /path/to/cargo-metadata.json
The registry paths in that document must be readable on this machine.
"""
import hashlib
import json
from pathlib import Path
import shutil
import sys

root = Path(__file__).resolve().parents[1]
metadata = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
destination = root / "LICENSES" / "dependencies"
destination.mkdir(parents=True, exist_ok=True)
inventory = []
missing = []
for package in sorted(metadata["packages"], key=lambda p: (p["name"], p["version"])):
    if package["source"] is None:
        continue
    source = Path(package["manifest_path"]).parent.resolve()
    folder = destination / f'{package["name"]}-{package["version"]}'
    files = []
    candidates = set()
    for path in source.rglob("*"):
        if path.is_file() and path.name.upper().startswith(("LICENSE", "LICENCE", "COPYING", "NOTICE")):
            candidates.add(path)
    if package.get("license_file"):
        candidates.add(source / package["license_file"])
    for path in sorted(candidates):
        if not path.resolve().is_relative_to(source) or not path.is_file():
            continue
        relative = path.relative_to(source)
        output = folder / relative
        output.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(path, output)
        files.append({"path": output.relative_to(root).as_posix(), "sha256": hashlib.sha256(output.read_bytes()).hexdigest()})
    if not files:
        missing.append(package["name"] + "-" + package["version"])
    inventory.append({"name": package["name"], "version": package["version"], "source": package["source"], "authors": package.get("authors", []), "license": package["license"], "repository": package.get("repository"), "files": files})
(destination / "index.json").write_text(json.dumps({"schema_version": 1, "packages": inventory, "without_bundled_license_file": missing}, indent=2) + "\n", encoding="utf-8")
print(json.dumps({"packages": len(inventory), "license_files": sum(len(p["files"]) for p in inventory), "without_bundled_license_file": missing}))

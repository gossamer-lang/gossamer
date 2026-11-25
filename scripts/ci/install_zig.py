"""Install a Zig release for CI and put it on the job's PATH.

Usage: install_zig.py VERSION DEST

The official index (ziglang.org/download/index.json) names the archive for
this host and its SHA-256. The archive itself comes from Zig's community
mirrors, which the Zig project asks automated downloads to use, with
ziglang.org as the last source; an archive whose hash does not match the
index is discarded, so a mirror never decides what is installed.
"""

import hashlib
import io
import json
import os
import platform
import random
import sys
import tarfile
import urllib.request
import zipfile

INDEX = "https://ziglang.org/download/index.json"
MIRRORS = "https://ziglang.org/download/community-mirrors.txt"
SOURCE = "gossamer-ci"


def fetch(url, timeout=120):
    request = urllib.request.Request(url, headers={"User-Agent": SOURCE})
    with urllib.request.urlopen(request, timeout=timeout) as response:
        return response.read()


def host_key():
    machine = platform.machine().lower()
    arch = {"x86_64": "x86_64", "amd64": "x86_64", "aarch64": "aarch64", "arm64": "aarch64"}.get(
        machine
    )
    system = {"linux": "linux", "darwin": "macos", "windows": "windows"}.get(
        platform.system().lower()
    )
    if arch is None or system is None:
        sys.exit(f"install_zig: no Zig build for {platform.machine()} {platform.system()}")
    return f"{arch}-{system}"


def sources(file_name, official):
    try:
        mirrors = [m.strip() for m in fetch(MIRRORS, 30).decode().splitlines() if m.strip()]
    except OSError as err:
        print(f"install_zig: mirror list unavailable ({err}); using ziglang.org", file=sys.stderr)
        mirrors = []
    random.shuffle(mirrors)
    return [f"{m.rstrip('/')}/{file_name}?source={SOURCE}" for m in mirrors] + [official]


def extract(archive, file_name, dest):
    os.makedirs(dest, exist_ok=True)
    if file_name.endswith(".zip"):
        with zipfile.ZipFile(io.BytesIO(archive)) as zf:
            zf.extractall(dest)
    else:
        with tarfile.open(fileobj=io.BytesIO(archive), mode="r:xz") as tf:
            tf.extractall(dest, filter="data")
    (top,) = [e for e in os.listdir(dest) if e.startswith("zig-")]
    return os.path.join(dest, top)


def main():
    version, dest = sys.argv[1], sys.argv[2]
    entry = json.loads(fetch(INDEX))[version][host_key()]
    official, want = entry["tarball"], entry["shasum"]
    file_name = official.rsplit("/", 1)[1]
    for url in sources(file_name, official):
        try:
            archive = fetch(url)
        except OSError as err:
            print(f"install_zig: {url}: {err}", file=sys.stderr)
            continue
        got = hashlib.sha256(archive).hexdigest()
        if got != want:
            print(f"install_zig: {url}: sha256 {got}, index says {want}", file=sys.stderr)
            continue
        home = extract(archive, file_name, dest)
        print(f"install_zig: Zig {version} from {url} in {home}")
        github_path = os.environ.get("GITHUB_PATH")
        if github_path:
            with open(github_path, "a", encoding="utf-8") as out:
                out.write(home + "\n")
        return
    sys.exit(f"install_zig: no source served {file_name} with the published hash")


if __name__ == "__main__":
    main()

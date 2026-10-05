"""Prepare pinned upstream binaries and corresponding source for Cargo.

No package manager or system installation is involved. All outputs are in
Cargo's OUT_DIR. The package hash is published in Proton's GitHub release.
"""
import hashlib
import io
import gzip
import json
import lzma
import os
from pathlib import Path
import sys
import tarfile
import urllib.request

VERSION = "3.27.0"
PACKAGE_HASH = "4ae1f9a38392379a08ab6cfb61b44078a339f55d1d366bbba0a0d9acf901e900"
SOURCE_HASH = "9e866cba24bc646f19de8bf051434c54d15a11550ebc6c6170fa8100269453aa"


def verified_download(url, expected, destination, cache_name):
    cache = os.environ.get("SPOTTY_PROTON_BUNDLE_CACHE")
    candidates = [destination]
    if cache:
        if not Path(cache).is_absolute():
            raise ValueError("SPOTTY_PROTON_BUNDLE_CACHE must be absolute")
        candidates.append(Path(cache) / cache_name)
    for path in candidates:
        if path.is_file():
            data = path.read_bytes()
            if hashlib.sha256(data).hexdigest() == expected:
                return data
    request = urllib.request.Request(url, headers={"User-Agent": "Spotty-native-build"})
    with urllib.request.urlopen(request, timeout=120) as response:
        data = response.read()
    if hashlib.sha256(data).hexdigest() != expected:
        raise ValueError("Proton Bridge download failed SHA-256 verification")
    temporary = destination.with_suffix(".partial")
    temporary.write_bytes(data)
    temporary.replace(destination)
    return data


def data_archive(package):
    stream = io.BytesIO(package)
    if stream.read(8) != b"!<arch>\n":
        raise ValueError("Invalid Debian archive")
    while header := stream.read(60):
        if len(header) != 60 or header[58:] != b"`\n":
            raise ValueError("Invalid archive member")
        name = header[:16].decode().strip().rstrip("/")
        length = int(header[48:58].decode().strip())
        data = stream.read(length)
        if len(data) != length:
            raise ValueError("Truncated archive member")
        if length % 2:
            stream.read(1)
        if name.startswith("data.tar."):
            return data if name == "data.tar.gz" else gzip.compress(lzma.decompress(data), mtime=0)
    raise ValueError("No native Bridge payload in archive")


def main():
    destination = Path(sys.argv[1])
    package = verified_download(
        f"https://github.com/ProtonMail/proton-bridge/releases/download/v{VERSION}/protonmail-bridge_{VERSION}-1_amd64.deb",
        PACKAGE_HASH, destination / "bridge.deb", "bridge.deb",
    )
    source = verified_download(
        f"https://github.com/ProtonMail/proton-bridge/archive/refs/tags/v{VERSION}.tar.gz",
        SOURCE_HASH, destination / "source.tar.gz", "source.tar.gz",
    )
    (destination / "bridge-payload.tar.gz").write_bytes(data_archive(package))
    (destination / "bridge-source.tar.gz").write_bytes(source)
    with tarfile.open(destination / "bridge-dependencies.tar.gz", "w:gz") as output:
        for item in json.loads(Path(__file__).with_name("native_dependencies.json").read_text()):
            data = verified_download(item["url"], item["sha256"], destination / item["name"], item["name"])
            if item["kind"] == "source":
                info = tarfile.TarInfo("source/" + item["name"])
                info.size = len(data)
                info.mode = 0o644
                output.addfile(info, io.BytesIO(data))
                continue
            with tarfile.open(fileobj=io.BytesIO(data_archive(data)), mode="r:gz") as package:
                for member in package:
                    if ".so" in member.name and (member.isfile() or member.issym()):
                        contents = package.extractfile(member).read() if member.isfile() else None
                        member.name = "runtime-libs/" + Path(member.name).name
                        output.addfile(member, io.BytesIO(contents) if contents is not None else None)
                    elif member.isfile() and Path(member.name).name == "copyright":
                        contents = package.extractfile(member).read()
                        member.name = "runtime-licences/" + item["name"] + ".copyright"
                        output.addfile(member, io.BytesIO(contents))


if __name__ == "__main__":
    main()

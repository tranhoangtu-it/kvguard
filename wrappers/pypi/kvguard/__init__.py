"""Thin launcher for the kvguard native binary.

On first use the binary for this platform is downloaded from GitHub Releases, verified against
its published SHA-256 checksum and cached per version. Only the standard library is used.
"""

from __future__ import annotations

import hashlib
import os
import platform
import sys
import tarfile
import tempfile
import urllib.request
import zipfile
from pathlib import Path

__version__ = "0.3.0"

REPO = "tranhoangtu-it/kvguard"
TARGETS = {
    ("linux", "x86_64"): "x86_64-unknown-linux-musl",
    ("linux", "aarch64"): "aarch64-unknown-linux-musl",
    ("linux", "arm64"): "aarch64-unknown-linux-musl",
    ("darwin", "x86_64"): "x86_64-apple-darwin",
    ("darwin", "arm64"): "aarch64-apple-darwin",
    ("windows", "amd64"): "x86_64-pc-windows-msvc",
    ("windows", "x86_64"): "x86_64-pc-windows-msvc",
}
DOWNLOAD_TIMEOUT_SECONDS = 120


def _target() -> str:
    key = (platform.system().lower(), platform.machine().lower())
    try:
        return TARGETS[key]
    except KeyError:
        raise RuntimeError(f"kvguard has no prebuilt binary for {key[0]}-{key[1]}") from None


def _cache_dir() -> Path:
    if sys.platform == "win32":
        root = Path(os.environ.get("LOCALAPPDATA", Path.home() / "AppData" / "Local"))
    else:
        root = Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache"))
    return root / "kvguard" / __version__


def binary_path() -> Path:
    name = "kvguard.exe" if sys.platform == "win32" else "kvguard"
    return _cache_dir() / name


def _download(url: str) -> bytes:
    request = urllib.request.Request(url, headers={"User-Agent": "kvguard-pypi-launcher"})
    with urllib.request.urlopen(request, timeout=DOWNLOAD_TIMEOUT_SECONDS) as response:
        return response.read()


def _safe_extract_zip(archive: Path, dest: Path) -> None:
    root = dest.resolve()
    with zipfile.ZipFile(archive) as zf:
        for member in zf.namelist():
            if not (dest / member).resolve().is_relative_to(root):
                raise RuntimeError(f"unsafe path in archive: {member}")
        zf.extractall(dest)


def ensure_binary() -> Path:
    """Return the cached binary path, downloading and verifying it when missing."""
    binary = binary_path()
    if binary.exists():
        return binary

    target = _target()
    ext = "zip" if target.endswith("windows-msvc") else "tar.gz"
    asset = f"kvguard-v{__version__}-{target}.{ext}"
    base = f"https://github.com/{REPO}/releases/download/v{__version__}"
    archive_bytes = _download(f"{base}/{asset}")
    expected = _download(f"{base}/{asset}.sha256").decode().split()[0].lower()
    actual = hashlib.sha256(archive_bytes).hexdigest()
    if actual != expected:
        raise RuntimeError(f"checksum mismatch for {asset}: expected {expected}, got {actual}")

    dest = _cache_dir()
    dest.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory() as tmp:
        archive = Path(tmp) / asset
        archive.write_bytes(archive_bytes)
        if ext == "zip":
            _safe_extract_zip(archive, dest)
        else:
            with tarfile.open(archive) as tf:
                tf.extractall(dest, filter="data")
    if sys.platform != "win32":
        binary.chmod(0o755)
    return binary

"""`kvguard` console entry point: run the native binary with the same arguments."""

from __future__ import annotations

import subprocess
import sys

from kvguard import ensure_binary


def main() -> int:
    try:
        binary = ensure_binary()
    except Exception as exc:  # surface install problems as a clean CLI error
        print(f"kvguard: {exc}", file=sys.stderr)
        return 1
    try:
        return subprocess.call([str(binary), *sys.argv[1:]])
    except KeyboardInterrupt:
        return 130


if __name__ == "__main__":
    sys.exit(main())

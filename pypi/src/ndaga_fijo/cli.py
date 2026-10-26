"""`ndaga-fijo` command: launch the bundled macOS app, or copy it to Applications."""

import argparse
import os
import platform
import shutil
import subprocess
import sys
from pathlib import Path

from . import __version__

APP_NAME = "NdagaFijo.app"


def app_path() -> Path:
    return Path(__file__).resolve().parent / APP_NAME


def _ensure_executable(app: Path) -> None:
    exe = app / "Contents" / "MacOS" / "NdagaFijo"
    if exe.exists() and not os.access(exe, os.X_OK):
        exe.chmod(exe.stat().st_mode | 0o111)


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(
        prog="ndaga-fijo",
        description="Create bootable USB drives on macOS. Launches the Ndaga Fijo app.",
    )
    parser.add_argument("--version", action="version", version=f"ndaga-fijo {__version__}")
    parser.add_argument("--path", action="store_true", help="print where the app bundle is installed and exit")
    parser.add_argument(
        "--install",
        action="store_true",
        help="copy the app to /Applications (or ~/Applications if that is not writable)",
    )
    args = parser.parse_args(argv)

    if platform.system() != "Darwin":
        print("Ndaga Fijo is a macOS app; it can't run on " + platform.system() + ".", file=sys.stderr)
        return 1

    app = app_path()
    if not app.is_dir():
        print(f"The app bundle is missing from this install ({app}). Reinstall with: pip install --force-reinstall ndaga-fijo", file=sys.stderr)
        return 1
    _ensure_executable(app)

    if args.path:
        print(app)
        return 0

    if args.install:
        for base in (Path("/Applications"), Path.home() / "Applications"):
            try:
                base.mkdir(exist_ok=True)
                dest = base / APP_NAME
                if dest.exists():
                    shutil.rmtree(dest)
                shutil.copytree(app, dest, symlinks=True)
                print(f"Installed to {dest}")
                return 0
            except OSError:
                continue
        print("Could not copy the app to /Applications or ~/Applications.", file=sys.stderr)
        return 1

    return subprocess.call(["open", str(app)])


if __name__ == "__main__":
    raise SystemExit(main())

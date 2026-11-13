#!/usr/bin/env python3
"""Extract an installed WorkBuddy version or run the loopback capture listener.

Capture must run on the desktop's own machine. Before running it, isolate the
client from external networks, use a disposable profile with synthetic data,
and arrange for its target request to reach the loopback listener. This tool
does not configure those OS-specific protections or modify the client.
"""

import argparse
import json
import os
from pathlib import Path
import plistlib
import struct
import subprocess
import sys


def installed_version(client: Path) -> str:
    if client.suffix.lower() == ".app":
        with (client / "Contents" / "Info.plist").open("rb") as stream:
            metadata = plistlib.load(stream)
        version = metadata.get("CFBundleShortVersionString")
    elif client.suffix.lower() == ".exe" and sys.platform == "win32":
        # Keep the installation path out of the PowerShell source text.
        environment = os.environ.copy()
        environment["WORKBUDDY_CAPTURE_CLIENT"] = str(client)
        version = subprocess.check_output(
            ["powershell", "-NoProfile", "-NonInteractive", "-Command",
             "(Get-Item -LiteralPath $env:WORKBUDDY_CAPTURE_CLIENT).VersionInfo.ProductVersion"],
            text=True, env=environment,
        ).strip()
    elif client.is_dir():
        # Electron ASAR stores the application package.json in its indexed data.
        with (client / "resources" / "app.asar").open("rb") as stream:
            prefix = stream.read(16)
            if len(prefix) != 16:
                raise ValueError("truncated ASAR header")
            marker, header_size, _, json_size = struct.unpack("<4I", prefix)
            if marker != 4 or header_size < 8 or json_size > header_size - 8 or header_size > 64 * 1024 * 1024:
                raise ValueError("invalid ASAR header")
            header = json.loads(stream.read(json_size))
            entry = header["files"]["package.json"]
            size = entry["size"]
            offset = int(entry["offset"])
            if not isinstance(size, int) or not 0 < size <= 1024 * 1024 or offset < 0:
                raise ValueError("invalid ASAR package.json entry")
            stream.seek(8 + header_size + offset)
            metadata = json.loads(stream.read(size))
            if metadata.get("name") != "@genie/workbuddy-desktop":
                raise ValueError("ASAR package is not WorkBuddy desktop")
            version = metadata.get("version")
    else:
        raise ValueError("use a WorkBuddy installation or extracted application directory, macOS .app, or Windows .exe on Windows")
    if not isinstance(version, str) or not version.strip():
        raise ValueError("client does not expose a product version")
    return version.strip()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--client", required=True, type=Path, help="Linux installation or extracted Windows application directory, macOS .app, or Windows .exe")
    subcommands = parser.add_subparsers(dest="command", required=True)
    subcommands.add_parser("version", help="read the installed product version")
    capture = subcommands.add_parser("capture", help="run the host loopback capture-client")
    capture.add_argument("--capture-tool", required=True, type=Path, help="built stravia-tools executable")
    capture.add_argument("--ca", required=True, type=Path, help="temporary public CA output")
    capture.add_argument("--output", required=True, type=Path, help="sanitized capture JSON output")
    capture.add_argument("--port", type=int, default=0, help="loopback port; 0 selects an ephemeral port")
    capture.add_argument(
        "--isolated-client", action="store_true", required=True,
        help="confirm external networking is blocked and a disposable synthetic profile is in use",
    )
    args = parser.parse_args()
    client = args.client.expanduser().resolve()
    try:
        version = installed_version(client)
    except (OSError, ValueError, KeyError, TypeError, plistlib.InvalidFileException, subprocess.CalledProcessError) as error:
        parser.error(str(error))
    if args.command == "version":
        print(version)
        return
    expected = {".app": "darwin", ".exe": "win32"}.get(client.suffix.lower(), "linux")
    if sys.platform != expected:
        parser.error("capture must run on the same OS as the installed WorkBuddy desktop")
    if not 0 <= args.port <= 65535:
        parser.error("--port must be between 0 and 65535")
    tool = args.capture_tool.expanduser().resolve()
    os.execv(str(tool), [str(tool), "capture-client", "--client", "WorkBuddy desktop",
                         "--client-version", version, "--ca", str(args.ca),
                         "--output", str(args.output), "--port", str(args.port)])


if __name__ == "__main__":
    main()

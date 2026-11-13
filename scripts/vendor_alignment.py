#!/usr/bin/env python3
"""Run the shared vendor-alignment harness from a StraviaPlatform checkout."""

import argparse
import os
from pathlib import Path
import sys


def main() -> None:
    parser = argparse.ArgumentParser(add_help=False)
    repo = Path(__file__).resolve().parents[1]
    parser.add_argument(
        "--component", type=Path,
        default=repo / "target/wasm32-wasip2/release/stravia_vendor_workbuddy.wasm",
    )
    parser.add_argument(
        "--host-repo",
        type=Path,
        default=Path(os.environ["STRAVIA_PLATFORM_DIR"])
        if os.environ.get("STRAVIA_PLATFORM_DIR")
        else Path(__file__).resolve().parents[2] / "StraviaPlatform",
    )
    args, forwarded = parser.parse_known_args()
    host_repo = args.host_repo.expanduser().resolve()
    harness = host_repo / "tests/common/vendor_alignment.py"
    if not harness.is_file():
        parser.error(
            f"shared alignment harness not found: {harness}; "
            "set --host-repo or STRAVIA_PLATFORM_DIR to the host checkout"
        )
    os.environ["STRAVIA_PLATFORM_DIR"] = str(host_repo)
    os.execv(sys.executable, [
        sys.executable, str(harness), "--vendor", "workbuddy",
        "--component", str(args.component),
        "--response-code-map", str(repo / "tests/fixtures/response-code-map.json"),
        *forwarded,
    ])


if __name__ == "__main__":
    main()

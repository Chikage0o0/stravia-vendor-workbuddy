#!/usr/bin/env python3
"""构建并签名 WorkBuddy 插件市场 v2 分发资产。"""

import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import tomllib


ROOT = Path(__file__).resolve().parent.parent
CATALOG = "stravia-plugin-catalog.json"
PUBLIC = "stravia-plugin-catalog.pub"
SIGNATURE = CATALOG + ".minisig"
LICENSES = ("LICENSE", "NOTICE", "rust_decimal.LICENSE")


def required(name):
    value = os.environ.get(name, "")
    if not value.strip():
        raise ValueError(f"必须设置 {name}")
    return value


def main():
    tool = required("STRAVIA_TOOLS")
    host = required("HOST_VERSION_REQ")
    repository = required("REPOSITORY_ID")
    tag = required("RELEASE_TAG")
    timestamp = required("PUBLISHED_AT")
    version = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["package"]["version"]
    if repository != "1389031614" or tag != "v" + version:
        raise ValueError("仓库 ID 或 Release tag 与本项目不匹配")
    key = Path(required("PLUGIN_SIGNING_KEY_FILE")).resolve(strict=True)
    public = Path(required("PLUGIN_PUBLIC_KEY_FILE")).resolve(strict=True)
    password = required("PLUGIN_SIGNING_PASSWORD")
    component = (ROOT / "target/wasm32-wasip2/release/stravia_vendor_workbuddy.wasm").resolve(strict=True)
    tool_path = shutil.which(tool)
    if tool_path is None:
        raise ValueError(f"找不到 STRAVIA_TOOLS: {tool}")
    tool_path = str(Path(tool_path).resolve())

    with tempfile.TemporaryDirectory(prefix="workbuddy-dist-") as tmp:
        tmp = Path(tmp)
        declarations = tmp / "input.json"
        declarations.write_text(json.dumps([{
            "file": str(component),
            "vendor_id": "workbuddy",
            "version": version,
            "display_name": "WorkBuddy",
        }]), encoding="utf-8")
        output = tmp / "output"
        subprocess.run([
            tool_path, "plugin-catalog", "--repository-id", repository,
            "--release-tag", tag, "--host-version-req", host,
            "--published-at", timestamp, "--input", str(declarations),
            "--output", str(output),
        ], check=True)
        catalog = json.loads((output / CATALOG).read_bytes())
        if (catalog["repository_id"] != int(repository)
                or catalog["release_tag"] != tag or len(catalog["releases"]) != 1):
            raise ValueError("目录身份不匹配")
        artifact = catalog["releases"][0]["artifact"]
        name = artifact["filename"]
        if name != f"stravia-vendor-workbuddy-v{version}.wasm.zst":
            raise ValueError("目录资产文件名不匹配")
        data = (output / name).read_bytes()
        if len(data) != artifact["size"] or hashlib.sha256(data).hexdigest() != artifact["sha256"]:
            raise ValueError("压缩资产摘要或大小不匹配")
        shutil.copyfile(public, output / PUBLIC)
        subprocess.run([
            "minisign", "-S", "-s", str(key), "-m", str(output / CATALOG),
            "-x", str(output / SIGNATURE),
        ], input=(password + "\n").encode(), check=True)
        subprocess.run([
            "minisign", "-V", "-p", str(output / PUBLIC),
            "-m", str(output / CATALOG), "-x", str(output / SIGNATURE),
        ], check=True)
        for license_name in LICENSES:
            source = ROOT / "vendor" / license_name if license_name == "rust_decimal.LICENSE" else ROOT / license_name
            shutil.copyfile(source, output / license_name)

        dist = ROOT / "dist"
        dist.mkdir(exist_ok=True)
        # 验签通过后才替换分发资产；仅清理旧入口生成的原始组件与校验清单。
        for obsolete in dist.glob("stravia-vendor-workbuddy-v*.wasm"):
            obsolete.unlink()
        (dist / "SHA256SUMS").unlink(missing_ok=True)
        for filename in (name, CATALOG, PUBLIC, SIGNATURE, *LICENSES):
            shutil.copyfile(output / filename, dist / filename)
            print(f"dist/{filename}")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as exc:
        print(f"发布打包失败: {exc}", file=sys.stderr)
        sys.exit(1)

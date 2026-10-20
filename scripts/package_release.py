"""Package only distributable files; never include launcher or game state."""

import argparse
import hashlib
import json
import platform
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--platform", required=True, choices=[
        "windows-x64", "linux-x64", "linux-arm64", "macos-x64", "macos-arm64",
        "macos-universal"
    ])
    args = parser.parse_args()
    host_os = {"Windows": "windows", "Linux": "linux", "Darwin": "macos"}.get(platform.system())
    host_arch = {"amd64": "x64", "x86_64": "x64", "aarch64": "arm64", "arm64": "arm64"}.get(
        platform.machine().lower()
    )
    universal = args.platform == "macos-universal" and host_os == "macos"
    if not universal and args.platform != f"{host_os}-{host_arch}":
        raise SystemExit("Platform must match the native build host; cross-packaging is not supported.")
    root = Path(__file__).resolve().parents[1]
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"],
        cwd=root, text=True
    ))
    package = next(p for p in metadata["packages"]
                   if Path(p["manifest_path"]).resolve() == root / "Cargo.toml")
    name, version = package["name"], package["version"]
    windows = args.platform.startswith("windows-")
    binary = name + (".exe" if windows else "")
    executable = Path(metadata["target_directory"]) / "release" / binary
    if not executable.is_file():
        raise SystemExit(f"Build first: missing {executable}")
    if universal:
        subprocess.run(
            ["lipo", str(executable), "-verify_arch", "arm64", "x86_64"], check=True
        )
    dist = root / "dist"
    dist.mkdir(exist_ok=True)
    stem = f"{name}-v{version}-{args.platform}"
    output = dist / (stem + (".exe" if windows else ".tar.gz"))
    if windows:
        shutil.copy2(executable, output)
    else:
        with tempfile.TemporaryDirectory(prefix="launcher-package-") as temp:
            stage = Path(temp) / stem
            stage.mkdir()
            shutil.copy2(executable, stage / binary)
            shutil.copy2(root / "run.sh", stage / "run.sh")
            (stage / binary).chmod(0o755)
            (stage / "run.sh").chmod(0o755)
            with tarfile.open(output, "w:gz") as archive:
                archive.add(stage, arcname=stem)
    with output.open("rb") as archive_file:
        digest = hashlib.file_digest(archive_file, "sha256").hexdigest()
    output.with_name(output.name + ".sha256").write_text(
        f"{digest}  {output.name}\n", encoding="utf-8"
    )
    print(output)


if __name__ == "__main__":
    main()

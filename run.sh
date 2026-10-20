#!/bin/sh
set -eu

# Keep portable configuration and relative game paths beside the launcher.
launcher_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
cd "$launcher_dir"
for launcher_binary in ./tui-launcher ./target/release/tui-launcher; do
    if [ -f "$launcher_binary" ]; then
        if [ ! -x "$launcher_binary" ]; then
            printf '程序没有执行权限，请运行 chmod +x "%s"\n' "$launcher_binary" >&2
            exit 1
        fi
        exec "$launcher_binary" "$@"
    fi
done
if ! command -v cargo >/dev/null 2>&1; then
    printf '未找到启动器程序或 Rust。请先安装 Rust，再运行 cargo build --release。\n' >&2
    exit 1
fi
exec cargo run --locked --release -- "$@"

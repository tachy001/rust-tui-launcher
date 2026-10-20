# macOS / Linux

## 运行

在可写目录解压与机器架构一致的构建包，用终端运行：

```sh
sh run.sh
```

脚本优先运行随包提供的 `tui-launcher`，源码目录下则优先运行
`target/release/tui-launcher`；找不到程序才使用 Cargo 构建。
脚本将工作目录固定为自身目录，配置 `launcher.json` 和默认 `.minecraft`
保存在这里。传入的相对路径也以这里为基准；导入其他目录的文件建议使用绝对路径。
可直接运行 `./tui-launcher`，但这时配置位置由当前工作目录决定。
终端需支持 UTF-8，界面至少 48 × 16。

## 从源码构建

安装当前稳定版 Rust 和系统 C 编译工具。macOS 可通过
`xcode-select --install` 安装 Command Line Tools。
Debian / Ubuntu 的构建依赖为：

```sh
sudo apt-get install build-essential pkg-config libdbus-1-dev
cargo build --locked --release
sh run.sh
```

Fedora 对应 `gcc`、`pkgconf-pkg-config`、`dbus-devel`；
Arch 对应 `base-devel`、`pkgconf`、`dbus`。
这是 glibc Linux 构建方案，未提供 Alpine / musl 静态构建。

## Java 与游戏窗口

Java 探测包括 PATH、JAVA_HOME、JDK_HOME、启动器下载目录、
macOS 的系统及用户 JavaVirtualMachines、Intel / Apple Silicon Homebrew、
Linux 的 `/usr/lib/jvm` 等目录，以及用户的 SDKMAN 和 `.jdks`。
允许跟随安装目录链接，并限制扫描深度、数量和循环。

请使用与启动器架构一致的 Java。Apple Silicon 原生程序应配 ARM64 Java；
旧版本游戏或 Mod 如果只提供 Intel 本地库，需要自行准备完整一致的
Intel 启动器、Java 及相应兼容环境，不能只更换 Java。
Linux ARM64 可构建启动器，但 Mojang Java 自动下载目前未提供此平台映射；
需要自行安装 Java，游戏和 Mod 也必须有对应 ARM64 本地库。
这不意味着所有 Minecraft 历史版本都支持 ARM64。

macOS 的 LWJGL 3 游戏在元数据未提供时自动补充 `-XstartOnFirstThread`，
已有参数不重复添加；旧 LWJGL 2 不强行添加。
依据：[LWJGL GLFW 平台说明](https://javadoc.lwjgl.org/org/lwjgl/glfw/package-summary.html)。
解压的本地库按系统和 CPU 架构分目录保存，避免不同系统使用同一旧目录。

Linux 游戏窗口需要可用的图形桌面和 OpenGL 驱动；TUI 能在终端运行，
不表示无图形环境的 SSH 会话能直接显示 Minecraft 窗口。

## 登录凭证

Windows 使用系统凭证管理器，macOS 使用 Keychain，Linux 使用
D-Bus Secret Service。Linux 正版登录需要可用的用户 D-Bus 会话和已解锁的
密钥环服务，例如 GNOME Keyring 或支持 Secret Service 的 KeePassXC。
仅安装 D-Bus 开发库并不能提供密钥环服务。安全存储不可用时登录会报错，
不会将令牌降级写入普通配置文件；离线账户不依赖该服务。
微软 OAuth 客户端 ID 仍由用户在设置页填写。

## 验证范围

已增加 Windows x64、Linux x64、macOS 的 GitHub Actions 配置，
分别执行格式检查、静态检查、本地测试及发布构建。
macOS 在 Apple Silicon runner 上测试，并构建、合并 Intel / Apple Silicon
两个架构为 Universal 程序；Intel 架构只构建，不在该 runner 上执行测试。
Release 只上传三个系统包，Linux ARM64 需自行源码构建。
Unix 包使用 tar 保存执行权限。工作流需将项目上传 GitHub 后才会执行；
添加配置并不代表这些平台已经构建或运行成功。

当前开发机是 Windows。macOS / Linux 的实际构建、游戏图形与音频、
微软登录和密钥环交互仍需在对应系统验收。

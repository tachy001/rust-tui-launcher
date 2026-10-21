# Tui-Launcher

Rust 编写的 Minecraft Java 版终端启动器，使用 Ratatui + Crossterm。界面和后端分离，支持中文和纯键盘操作。

## 功能

版本变化见 [更新记录](CHANGELOG.md)。

- 本地版本发现、官方版本目录、正式版 / 快照筛选、名称搜索、上次选择记忆、离线目录缓存。
- 原版安装与修复；1–16 路并发、SHA-1 与大小校验、重试、HTTP Range 断点恢复、下载暂停 / 取消。
- 版本继承、Maven 坐标、库版本覆盖、平台规则、旧版 / 新版参数、原生库解压、日志配置。
- Fabric / Quilt 官方配置安装；Forge / NeoForge 官方命令行安装器入口。
- Java 自动发现与匹配、Mojang 官方 Java 运行时下载；自动模式缺少建议版本时尝试安装。
- 版本隔离与每实例设置：Java、内存、隔离方式、JVM / 游戏附加参数；全局窗口大小设置。
- Mod JAR 导入与启用 / 禁用；Modrinth `.mrpack` 与 CurseForge ZIP 导入新隔离实例，校验下载文件并合并 overrides。
- 多离线账户；可配置微软设备代码登录、Xbox / Minecraft 资格检查、令牌刷新与系统安全凭证存储。
- 实时游戏日志、历史日志查看、退出状态、明确确认后终止游戏、后台任务错误恢复。
- TUI、命令行、F1 操作帮助和原子配置保存。

**实现不等于全部实测。** Windows 上已实际安装并启动原版 1.21.1 和 Fabric 0.19.5，完成图形、声音与资源初始化；也已下载并运行 Mojang Java 21。Forge 52.1.20 与 NeoForge 21.1.264 已完成官方安装器、客户端补丁生成和后续资源校验，详见 [Forge 记录](docs/FORGE-VALIDATION.md)与 [NeoForge 记录](docs/NEOFORGE-VALIDATION.md)，尚未启动这两个游戏实例。微软真实登录及跨平台行为仍未验收；整合包已有本地 ZIP 导入与修复证据，真实平台联网导入仍需验证，详见 [ROADMAP.md](ROADMAP.md)。

## 启动

Windows 直接运行下载的 EXE；macOS / Linux 解压后在终端进入程序所在目录，运行 `./tui-launcher`。
Java 探测、macOS 图形启动参数和本地库目录已作兼容处理，支持分平台源码构建。
安装依赖、ARM64 限制与验证范围见 [跨平台说明](docs/PLATFORMS.md)。

从源码构建后的 Windows 程序可在项目目录直接运行：

```powershell
.\target\release\tui-launcher.exe
```

修改源码后执行 `cargo build --release`；macOS / Linux 构建后运行 `./target/release/tui-launcher`。
配置和相对路径以当前工作目录为基准；希望数据与程序放在一起时，先进入程序所在目录。

从源码启动需要稳定版 Rust、平台 C 链接器；Linux 的安全凭证后端还需要 D-Bus 开发库：

```console
cargo run --release
```

默认 Java 为 `auto`。一级入口只有账户、实例管理、实例列表、下载、设置。首次使用按 4 进入下载，R 获取版本，Enter 安装；完成后按 3 进入实例列表，Enter 管理所选实例，Space 启动。Java 检测位于设置中，Mod、存档和加载器等位于实例管理中。Esc 返回上一级。若使用已有 `.minecraft`，请在设置 › 全局游戏与下载设置中指定目录。默认隔离目录会将存档、Mod、配置放在 `versions/<实例ID>/`，不会自动迁移共享目录里的旧存档。

## 文档

- [使用指南：快捷键、命令行、配置与数据目录](docs/USAGE.md)
- [macOS / Linux 安装与平台限制](docs/PLATFORMS.md)
- [微软账户配置](docs/AUTHENTICATION.md) · [CurseForge 配置](docs/CURSEFORGE.md)
- [GitHub 上传、打包与 Release](docs/RELEASING.md)
- [更新记录](CHANGELOG.md) · [开发与验收计划](ROADMAP.md)

## 项目结构

```text
src/                 Rust 界面与启动器后端
scripts/             发布打包工具
docs/                使用、平台、发布和验收文档
.github/workflows/   Windows / macOS / Linux 构建及 Release 草稿流程
Cargo.toml           项目信息与依赖
Cargo.lock           固定依赖版本，需提交 Git
```

`target/`、`.tools/`、`.tui-mc/`、游戏目录和本机配置不进入仓库或发布包。
项目由 TUI / MC 更名为 Tui-Launcher；已有 `.tui-mc/`、`tui-mc-*`
数据文件名和系统凭证服务标识保持兼容，避免丢失账户或实例管理记录。

## 许可证与参考

[MIT](LICENSE)。启动流程参考 HMCL、PCL，Rust 实现独立编写。
本项目与 Mojang / Microsoft 无隶属关系。

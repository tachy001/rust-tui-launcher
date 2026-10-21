# 使用指南

## 键盘

一级导航只保留五项：**1 账户、2 实例管理、3 实例列表、4 下载、5 设置**。宽终端显示左侧栏，窄终端显示顶部紧凑导航。

实例列表中 Enter 打开所选实例的管理菜单，Space 直接启动。管理菜单提供游戏设置、Mod、存档、加载器 / 组件、修复、Java 安装、诊断、日志、名称、复制、导出与回收；加载器菜单再选择 Fabric / Quilt / Forge / NeoForge。管理页面始终显示当前实例，Esc 返回上一级并保留原选项。

回收站、任务日志、Java 检测、整合包导入及 CurseForge 密钥入口位于设置菜单。下面保留原有快捷键作为熟练用户的补充入口。

| 按键 | 功能 |
| --- | --- |
| ↑↓（导航聚焦） | 选择一级入口 |
| Enter / →（导航聚焦） | 进入内容区 |
| ←（内容聚焦） | 返回导航栏；Alt＋左右仍用于日志横向滚动 |
| Tab / Shift+Tab | 切换导航栏与内容区焦点 |
| 1–5 | 直达一级入口并进入内容区 |
| ↑↓ / j k / PgUp PgDn | 选择；日志页逐行滚动 / 翻页 |
| Alt＋左右 | 日志页横向查看长行 |
| F6 | 打开当前任务进度浮窗 |
| ↑↓ / PgUp PgDn / Home End（结果窗口） | 滚动查看完整错误或任务结果 |
| Enter / Space（实例列表） | 进入所选实例管理 / 启动游戏 |
| Esc / Backspace | 返回上一级；浮窗、输入框优先关闭自身 |
| Enter（其他页面） | 打开子菜单、安装、编辑设置、切换账户或 Mod 状态 |
| / | 搜索版本；Ctrl+U 清空输入 |
| F / U | 安装 Fabric / Quilt |
| H / N | 在版本页安装 Forge / NeoForge |
| X / B / L | 本地页修复、安装建议 Java、查看日志 |
| O / M | 本地页打开实例设置 / Mod 管理 |
| F2 / T / E | 本地页修改显示名称 / 复制实例 / 导出整合包 |
| Delete | 实例列表移入回收站；设置 › 回收站查看与恢复 |
| I | 导入 Modrinth 整合包 |
| A | Mod 页导入 JAR；账户页开始微软登录 |
| Shift+K | 账户页安全配置自己的 CurseForge API 密钥 |
| N / D | 账户页添加离线账户 / 移除账户 |
| G / D | 设置页回到全局 / 清除实例覆盖 |
| P / C | 下载暂停继续 / 取消任务 |
| V / F1 / Q | Java 检测 / 帮助 / 空闲时退出 |

安装、下载等后台任务开始时自动显示进度浮窗，包含当前阶段、进度条、可用的速度和剩余时间，以及最新日志。进度未知时显示处理中。浮窗内按 P 暂停或继续可暂停的下载，C 取消，L 查看日志，Esc 隐藏浮窗并保持任务运行；任务结束后自动关闭。游戏运行默认保持后台，可按 F6 查看任务，取消游戏仍需确认。

取消普通下载会保留 `.part` 文件，下次安装继续校验和恢复；网络读取返回前可能需要等待。游戏终止需按 C 后再按 Y。官方 Forge / NeoForge 安装器自身的网络任务不能由本启动器逐文件暂停，取消会结束安装器进程。

## 命令行

```console
tui-launcher --help
tui-launcher doctor
tui-launcher remote 1.21 --snapshots
tui-launcher install 1.21.1
tui-launcher fabric 1.21.1
tui-launcher quilt 1.21.1
tui-launcher forge 1.21.1
tui-launcher neoforge 1.21.1
tui-launcher repair fabric-loader-0.19.5-1.21.1
tui-launcher launch 1.21.1
tui-launcher java-install 21
tui-launcher import MyPack.mrpack my-pack
tui-launcher pack-info MyPack.mrpack
tui-launcher mods my-pack add Example.jar
tui-launcher account offline Player
tui-launcher instance name 1.21.1 "我的原版"
tui-launcher instance clone 1.21.1 my-survival
tui-launcher export my-survival MyPack.mrpack
tui-launcher instance remove my-survival
tui-launcher instance trash
tui-launcher instance restore <回收站ID>
```

`--game-dir <路径>`、`--java <auto或完整路径>`、`--username <名字>`、`--memory <MB>`、`--threads <1-16>`、`--shared`、`--isolated` 可用于本次命令。命令行不会把这些临时覆盖值自动保存。CLI 的 Ctrl+C 取消下载或终止游戏。

下载源可在设置中填写 `official`、`bmclapi`，或兼容 BMCLAPI 路径的 HTTPS 根地址，也可用 `--source` 临时指定。镜像失败会回退原地址，所有原有文件校验保持生效，微软 / Xbox / Minecraft 登录请求不改写。整合包文件的多个 HTTPS 下载地址会依次尝试；用 `--no-optional` 或设置页可不安装整合包标记为可选的客户端文件。

账户操作和应用注册说明见 [docs/AUTHENTICATION.md](AUTHENTICATION.md)。当前没有内置 OAuth 客户端 ID，需自行配置；普通配置只保存账户索引，令牌由操作系统安全存储保管。未选择账户时沿用配置中的离线用户名。

## 配置、目录与日志

`launcher.json` 位于启动器工作目录，旧版四字段配置仍可加载。设置页保存时原子替换文件。Java 路径使用 `auto`、PATH 命令或完整可执行文件路径，不能附加参数或额外引号；附加参数填写到专门的 JVM / 游戏参数项。

游戏资源与库共享，存档等数据按隔离设置放置。整合包始终隔离；导入不覆盖已有完整实例，失败后保留标记，可以对同一包和实例名继续导入。新导入的实例有修复记录：删除或损坏 Mod 后可用 X / `repair` 修复，不改变已禁用 Mod 的状态，也不覆盖现有配置修改。仅在文件缺失时恢复配置原值。旧版导入且没有修复记录的实例仍需补回文件或重新导入。

CurseForge 查询需要自己的 API 密钥；API 不提供自动下载地址时可按缺失清单手动补齐。配置方法、缓存说明与验证范围见 [docs/CURSEFORGE.md](CURSEFORGE.md)。

实例显示名称与 ID 分开，改名称不会破坏依赖。复制实例会复制允许的游戏数据目录（含存档），保留启动设置，并以原实例为父版本共享依赖。复制失败可对同一目标 ID 继续。移除采用游戏根目录的 `tui-mc-trash/` 回收站，不永久删除；恢复时不会覆盖同名实例。实例锁放在独立的 `tui-mc-locks/` 目录，防止运行期间移动实例。

整合包导出生成标准索引与 `overrides/`，默认包含 Mod、配置、资源包等，不包含存档、截图、服务器地址以及启动器自身的账户和令牌；导出文件不会覆盖已有文件。未识别的自定义顶层目录不自动打包，避免把无关个人文件带入。导出的依赖会在导入时重新安装，备份不是完全离线的游戏发行包。

游戏日志：`<实际游戏目录>/tui-mc-game.log`，每次启动覆盖；游戏自身日志仍在 `logs/`。安装器日志保存在游戏根目录 `tui-mc-installers/`。账户索引、远程目录缓存位于 `.tui-mc/`。

## 当前限制与开发

尚未接入皮肤管理。在线 Mod 安装已有真实样例，完整依赖自动回溯仍待完善；CurseForge 的真实联网导入没有 API 密钥可验证。旧 Forge 安装器如果没有命令行客户端安装选项会报错，不保证全部历史版本兼容。ARM 原生库与不同操作系统的实际兼容性需要对应平台验收。镜像已实现路径映射和官方回退，网络兼容性仍需验证。

```console
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo build --release
```

默认测试不连接微软、不写入系统凭证、不启动 JVM 或游戏。需要外部服务的测试单独标记 `ignored`，只在相关流程发生变化并需要验收时手动运行。

后端流程参考了工作区 HMCL 的兼容规则、参数模型与启动流程，以及 PCL 的 classpath / native 参数组织；Rust 实现独立编写，没有复制 Java / VB 源码。参考项目：[HMCL](https://github.com/HMCL-dev/HMCL)、[PCL](https://github.com/Hex-Dragon/PCL2)。

# 后端源码对照记录

参考的是工作区 `../references/HMCL-main` 与 `../references/PCL-main` 的本地快照；下方上游链接便于定位，不代表已核对上游最新提交。规则按 Rust 服务职责独立实现，未直接移植 Java / VB 源码。

## Java 运行时兼容范围

- [HMCL JavaVersionConstraint.java](https://github.com/HMCL-dev/HMCL/blob/main/HMCLCore/src/main/java/org/jackhuang/hmcl/game/JavaVersionConstraint.java)：重点阅读 `GAME_JSON`、`LAUNCH_WRAPPER`、`MODDED_JAVA_*`、`MODLAUNCHER_8`。元数据最低要求和加载器兼容范围是不同约束；部分约束是强制限制，部分是建议。
- [PCL ModJava.vb](https://github.com/Hex-Dragon/PCL2/blob/main/Plain%20Craft%20Launcher%202/Modules/Minecraft/ModJava.vb)：重点阅读 `GetJavaRequirement` 和 `SelectOrDownloadJava`。按游戏和加载器计算范围，再选择 / 下载运行时；还处理 Forge 更新号、NeoForge 和旧 Fabric 的最高版本限制。

本项目原先只有“Java 主版本不少于元数据要求”，下载建议版本失败后可能回退到过新的 Java。现新增 `JavaRequirement` 并接入 `JavaRuntimeService`：识别 LaunchWrapper 启动主类及 `net.minecraft:launchwrapper` 的低于 1.13 的纯数字版本，禁止使用 Java 9+；自动模式可安装 Java 8，手动指定不兼容 Java 时明确报错，不擅自替换。元数据最低要求与该上限冲突时直接报告冲突。

此处依据实际主类和库身份判断，避免用可重命名的实例 ID 猜测游戏版本。未知 / 带限定符的库版本不按纯数字规则猜测。只存在旧库而使用其他主类时不施加该限制，LaunchWrapper 1.13+ 也不继承此限制。

8 项本地运行时策略检查通过，覆盖既有取消 / 回退策略和新增上下限处理，没有启动 Java 或游戏。真实历史 Forge 实例尚未验收。

尚未统一的规则：两份快照对于部分历史 Forge 的 Java 上限、补丁级别和建议 / 强制程度不同。本项目当前只保留主版本信息，不能声称已实现 Java 8u320 / 8u321 等更新号边界。后续须先扩充运行时版本模型，再按具体加载器版本逐项核对；不能把所有旧 Forge 简化成同一个最高版本。

本次读取文件的 SHA-256：

| 本地文件 | SHA-256 |
| --- | --- |
| `HMCL-main/HMCLCore/src/main/java/org/jackhuang/hmcl/game/JavaVersionConstraint.java` | `901F6E367852F39AE5D200D7B1CBD6E0979608017D486B6554CD64918169958E` |
| `PCL-main/Plain Craft Launcher 2/Modules/Minecraft/ModJava.vb` | `757AC11D8CE7174E89A0BAF14340ABF5AD5E1550AEAA4196C1A0A9034468E914` |

## classpath 与 native 组织

- [HMCL DefaultLauncher.java](https://github.com/HMCL-dev/HMCL/blob/main/HMCLCore/src/main/java/org/jackhuang/hmcl/launch/DefaultLauncher.java)：阅读 `getClasspath`、`generateCommands` 中库 / 客户端顺序、native 路径、日志参数及旧 BootstrapLauncher ignoreList 处理。classpath 使用保序集合，OptiFine 与 Forge / LiteLoader 共存还有专门处理。
- [PCL ModLaunch.vb](https://github.com/Hex-Dragon/PCL2/blob/main/Plain%20Craft%20Launcher%202/Modules/Minecraft/ModLaunch.vb)：阅读启动参数替换处的支持库列表和 `McLaunchNatives`。native 不进入普通 classpath；OptiFine 有位置要求，Windows 路径还有自身适配。

本项目已区分普通库和 native，并新增 `LaunchClasspath`：按首次出现顺序对相同库路径去重，客户端 JAR 最后加入一次，使用目标系统的 classpath 分隔符。无法表示的 Unicode 路径和含分隔符的路径明确报错，避免静默变成多个错误条目。PCL 的 Windows 专用短路径与分隔符不能直接用于 macOS / Linux。OptiFine 混装顺序、安装器与补丁 JAR 的选择仍未实现，不能只移动文件顺序便宣称支持混装。

进一步阅读 [HMCL LaunchManifestNormalizer.java](https://github.com/HMCL-dev/HMCL/blob/main/HMCLCore/src/main/java/org/jackhuang/hmcl/game/LaunchManifestNormalizer.java) 的 `repairBootstrapLauncher`，与 `DefaultLauncher.rewriteUnsafeBootstrapLauncherIgnoreList` 对照：0.1.17+ 按文件名忽略，旧版对完整路径做子串匹配，两者不能套用同一修复。

新增 `BootstrapCompatibility`，只对 BootstrapLauncher 主类、当前平台的 Forge / NeoForge 库和已知纯数字 BootstrapLauncher 版本生效。在 JVM 参数完成占位符展开后处理，游戏参数不参与：

- 0.1.17+ 保留既有忽略项，补上实际客户端 JAR 名称；继承实例不能用当前实例 ID 代替父版本客户端名。
- 旧版按库文件名匹配原始忽略项，再生成具体路径，并加入实际客户端路径，避免游戏目录名称包含 `asm` 等词时误忽略全部支持库。既有精确路径保持有效。
- 不向没有 ignoreList 的配置强加参数；未知限定符版本不猜测，当前平台存在互相矛盾的 BootstrapLauncher 版本时明确报错。逗号无法无歧义表示的路径报错，不截断路径扩大忽略范围。
- 补齐 `primary_jar`、`primary_jar_name`、`file_separator` 模板值；此批未修改实例元数据文件。

6 项本地策略检查通过；另只读加载工作区 NeoForge 21.1.264 的真实继承配置，确认忽略列表包含实际 `1.21.1.jar`。该检查不执行 Java，不证明新流程已经完成 NeoForge 游戏运行验收。

本批补充的本地文件 SHA-256：

| 本地文件 | SHA-256 |
| --- | --- |
| `HMCL-main/HMCLCore/src/main/java/org/jackhuang/hmcl/game/LaunchManifestNormalizer.java` | `FFB58D047348FB6F7EFF957746D54BD04626A72C9849C193B1FD0C5114CEA856` |
| `HMCL-main/HMCLCore/src/main/java/org/jackhuang/hmcl/launch/DefaultLauncher.java` | `A8E5123DC44A471639E6B6F92658CB3FC0204F86813BCD1E947EFAF84C8ACD51` |
| `PCL-main/Plain Craft Launcher 2/Modules/Minecraft/ModLaunch.vb` | `79BBA0379C61EC3936000B2F6A30C210937C0C5FDA8F060AC1ED1B480CF8C13E` |

## native 生命周期与解压失败

对照同一份 `DefaultLauncher.decompressNatives` 和 PCL `ModLaunch.McLaunchNatives`：HMCL 清理目标目录后按库排除规则解压，PCL 在解压后移除多余文件，并考虑 Windows 已加载 DLL 的占用。本项目原先复用同一目录逐文件覆盖，切换组件可能残留旧库，失败也留下部分解压结果。

新增 `NativeDirectory`，每次启动在平台 native 目录下创建独立的 `run-*` 子目录；提取完成后才传给后续启动参数。目录由当前启动会话持有，游戏进程先退出 / 回收，再释放目录。解压失败后该对象不可再提供可用路径；失败、取消及正常结束均尝试清理自己创建的目录，不操作其他运行会话或旧缓存。

规则保留所有平台的文件类型，不采用 PCL 仅提取 Windows DLL 的分支；继续处理 META-INF 与 metadata exclude，统一 ZIP 路径分隔符，拒绝越界路径及符号链接。逐文件和每 64 KiB 读取检查取消 / 暂停。5 项真实本地 ZIP 检查通过，覆盖 DLL / SO / dylib、排除规则、部分解压失败、并行目录隔离、取消、坏 ZIP 和符号链接条目；未运行 Java / 游戏。OS 强制终止、目录清理失败和各平台实际 DLL / SO 加载仍需验收，清理为尽力执行，不宣称崩溃后自动回收全部旧目录。

## 后续对照方式

后端改动先定位对应源码中的行为、适用版本和失败处理，再核对本项目差异；记录采用规则及未采用原因。优先继续对照 Java 完整版本、Forge / NeoForge 安装及依赖修复、认证刷新和实例隔离。保持服务对象与可替换依赖，不因参考其他启动器而大改 TUI。

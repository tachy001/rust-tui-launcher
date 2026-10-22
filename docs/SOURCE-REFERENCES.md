# 后端源码对照记录

参考的是工作区 `../references/HMCL-main` 与 `../references/PCL-main` 的本地快照；下方上游链接便于定位，不代表已核对上游最新提交。规则按 Rust 服务职责独立实现，未直接移植 Java / VB 源码。

## Java 运行时兼容范围

- [HMCL JavaVersionConstraint.java](https://github.com/HMCL-dev/HMCL/blob/main/HMCLCore/src/main/java/org/jackhuang/hmcl/game/JavaVersionConstraint.java)：重点阅读 `GAME_JSON`、`LAUNCH_WRAPPER`、`MODDED_JAVA_*`、`MODLAUNCHER_8`。元数据最低要求和加载器兼容范围是不同约束；部分约束是强制限制，部分是建议。
- [PCL ModJava.vb](https://github.com/Hex-Dragon/PCL2/blob/main/Plain%20Craft%20Launcher%202/Modules/Minecraft/ModJava.vb)：重点阅读 `GetJavaRequirement` 和 `SelectOrDownloadJava`。按游戏和加载器计算范围，再选择 / 下载运行时；还处理 Forge 更新号、NeoForge 和旧 Fabric 的最高版本限制。

本项目原先只有“Java 主版本不少于元数据要求”，下载建议版本失败后可能回退到过新的 Java。现新增 `JavaRequirement` 并接入 `JavaRuntimeService`：识别 LaunchWrapper 启动主类及 `net.minecraft:launchwrapper` 的低于 1.13 的纯数字版本，禁止使用 Java 9+；自动模式可安装 Java 8，手动指定不兼容 Java 时明确报错，不擅自替换。元数据最低要求与该上限冲突时直接报告冲突。

此处依据实际主类和库身份判断，避免用可重命名的实例 ID 猜测游戏版本。未知 / 带限定符的库版本不按纯数字规则猜测。只存在旧库而使用其他主类时不施加该限制，LaunchWrapper 1.13+ 也不继承此限制。

8 项本地运行时策略检查通过，覆盖既有取消 / 回退策略和新增上下限处理，没有启动 Java 或游戏。真实历史 Forge 实例尚未验收。

尚未统一的规则：两份快照对于部分历史 Forge 的 Java 上限、补丁级别和建议 / 强制程度不同。已新增完整版本模型，但 JavaRequirement 尚未实现 Java 8u320 / 8u321 等加载器更新号边界；仍需按具体加载器版本逐项核对，不能把所有旧 Forge 简化成同一个最高版本。

进一步阅读 [HMCL JavaManager.java](https://github.com/HMCL-dev/HMCL/blob/main/HMCL/src/main/java/org/jackhuang/hmcl/java/JavaManager.java) 的 `chooseJava`：优先接近游戏建议的主版本，同一主版本比较完整版本。对照 PCL `GetJavaRequirement` / `SelectOrDownloadJava` 可见完整版本还参与范围判断，单纯保留主版本不足以实现这些兼容规则。

本项目新增 `JavaVersion`，解析 Java 8 的 `1.8.0_321-b07` / `8u451` 与新版点分、`+build`、LTS 和预览格式；规范化数字部分以便比较，同时保留原版本文本。JavaRuntime 组合该对象，主版本由对象派生，避免主版本和完整版本分别保存后不一致。同主版本优先正式版，再按数字更新号和构建号选择较新候选；手动路径选择策略不变。探测输出按实际 Java / OpenJDK 版本头识别，跳过 JAVA_TOOL_OPTIONS 等警告，而非把输出第一行当作版本说明。

14 项相关本地检查通过，包括原有运行时策略检查，没有执行 JVM。此模型不把不同厂商的更新号强行映射为同一种补丁级别；完整加载器版本范围、架构检测和真实厂商输出适配仍需继续核验。

继续采用 PCL `GetJavaRequirement` 的已知 Fabric 规则：Loader 0.17.0 之前的 Mixin / ASM 不兼容 Java 25。本项目将该上限接入现有 `JavaRequirement`，根据 KnotClient 启动主类与当前平台启用的 `net.fabricmc:fabric-loader` 纯数字版本判断，而非实例 ID。旧版限制最高 Java 24；0.17.0+ 不套用此规则，未知限定符版本不猜测。元数据最低版本超过上限时报冲突，避免无效下载；手动 Java、安装结果及失败回退均不能绕过已知上限。

同时修正旧 LaunchWrapper 识别对平台规则的忽略：已禁用或不属于当前平台的库不施加兼容上限。本地选择和安装都失败时保留两份错误上下文，避免仅显示下载失败而丢失不兼容原因。11 项相关服务检查通过，无网络、JVM 或游戏执行；本项尚无旧 Fabric + Java 25 的真实游戏运行证据，其他 Forge 更新号规则保持未完成。

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

## 平台兼容规则

阅读 [HMCL CompatibilityRule.java](https://github.com/HMCL-dev/HMCL/blob/main/HMCLCore/src/main/java/org/jackhuang/hmcl/game/CompatibilityRule.java)、[OSRestriction.java](https://github.com/HMCL-dev/HMCL/blob/main/HMCLCore/src/main/java/org/jackhuang/hmcl/game/OSRestriction.java)、`Architecture.java`，以及 PCL `ModMinecraft.McJsonRuleCheck`。对照后保留按顺序应用最后一条匹配规则的行为，并新增 `PlatformRuleEvaluator` / `OsVersionSource`，通过注入 OS、架构和版本提供者本地检查不同平台。

修复原先把 `os.arch` 当作普通字符串比较的问题：支持完整正则匹配及 x86_64 / amd64 / x64、aarch64 / arm64 等别名；x86 不会误匹配 x86_64。支持常用平台名称别名及明确的 OS-架构组合。系统版本由生产提供者按需查询并缓存一次，失败也缓存为不可用；不可用版本不能用空字符串匹配 `.*`。

采用范围与差异：

- 空规则沿用 HMCL 和现有实现的默认允许行为；不采用 PCL 空列表默认禁止的差异。
- 架构正则按完整值匹配。版本正则保留现有 / PCL 的前缀与搜索匹配，避免将 `^10\.` 这类规则改成完整匹配后失效；不直接套用 HMCL 对版本的 `.matches()`。
- 不采用 PCL Windows 专用的非 Windows / ARM 排除逻辑。未知名称只有显式 `unknown` 视为无名称限制，其他不识别名称不猜测；附带架构 / 版本条件仍需满足。
- 未识别 action 不改变上一匹配结果；规则容器格式无效时不默认放行。空的可选 OS / features 按未设置处理，功能缺省 false 保持本项目原语义。
- Rust regex 不支持的 Java 正则扩展不会猜测执行，该条件不匹配；架构仍以启动器进程为准，尚未实现跨架构 Java 的完整 native 重选。

11 项相关本地检查通过，涵盖三端平台条件、架构正则 / 别名、版本不可用、规则顺序、功能标签及现有参数生成；测试使用替代版本提供者，不运行系统探测命令、Java 或游戏。实际系统版本探测及三端游戏运行验收仍单独保留。

## 账户状态提交与修改互斥

阅读 [HMCL Accounts.java](https://github.com/HMCL-dev/HMCL/blob/main/HMCL/src/main/java/org/jackhuang/hmcl/setting/Accounts.java) 中账户文件可写性、移除与覆盖入口，以及 PCL `ModLaunch` 的 OAuth 刷新、Xbox / Minecraft 分阶段认证和结果持久化。上游账户文件管理与本项目的系统安全凭证存储不同，不能直接照搬其保存方式或把上游单进程状态管理当作本项目跨进程保证。

本项目为 AccountIndexStore 增加必需的修改会话锁；生产通过 `.tui-mc/accounts.lock` 的 OS 排他锁保护读改写、凭证变化与回滚。锁冲突明确报错，不让两个进程各自用旧索引覆盖对方结果；锁文件保留，句柄释放即解锁。AccountService 的提前刷新令牌保存同样使用锁，并检查账户存在、类型与 OAuth 客户端配置，避免删除后重新写入凭证或旧客户端覆盖新客户端登录。

初批 16 项检查覆盖本地修改互斥，包含真实辅助子进程先被锁阻止、释放后成功进入的验证。随后新增 `RefreshSession`，让 OAuth、Xbox 与 Minecraft 流程共享同一账户的独立刷新锁；全局索引锁仅覆盖本地读写，其他账户刷新和账户选择不必等待网络。

索引增加默认 0 的 revision，账户增加默认 0 的 generation；新登录与成功刷新单调分配代际，删除保留 revision，拒绝删除后重加或同客户端重新登录后的旧请求。保存 OAuth 刷新结果和提交 Minecraft 结果两处都复查代际；同时比较当前安全存储与会话的凭证快照，防止新登录保存失败、已保留新刷新令牌但索引尚未更新时，旧会话仍覆盖该凭证。

本项目采用服务对象管理刷新生命周期，没有照搬上游的普通配置令牌保存方式。26 项相关检查通过，覆盖同账户互斥、其他账户独立运行、相同客户端 / 过期时间的重新登录、删除重加、失败提交、失败登录保留令牌、计数溢出及旧索引兼容；系统凭证由带账户命名空间的内存提供者替代，不访问真实密钥环或微软。不同工作目录不共享此锁及代际；崩溃时跨存储原子性和真实登录仍未验收。

## Java 探测超时与取消

继续阅读 [PCL PCLCS/Java.cs](https://github.com/Hex-Dragon/PCL2/blob/main/PCLCS/Java.cs) 的 `CheckAsync`：使用 15000 ms 的程序超时和调用方取消令牌；HMCL `JavaInfo.fromReleaseFile` 则展示了不执行程序时读取运行时元数据的另一种能力，两者不可直接视作相同的可执行性验证。

本项目原先用阻塞的 `Command.output()` 检查每个候选，异常程序可能永久阻塞发现。现由 `JavaProbe` 统一执行，保留现有 `-version` 检查，加入 15 秒运行超时、25 ms 轮询取消和 1 MiB 输出预算；游戏本身的等待策略不改变。自动发现会传播取消，不把取消当作无 Java；Forge / NeoForge 在尝试安装建议 Java 前也重新检查取消。CLI Java / doctor 安装 Ctrl+C 处理，TUI 仅接入探测服务的新接口。

stdout / stderr 使用本次探测的私有临时文件捕获，不使用需等待 EOF 的管道读线程；直接子进程退出或被终止后清理自己的捕获目录。Unix 目录 / 文件权限分别为 0700 / 0600。输出预算在轮询时检查，并限制最终内存读取，未宣称文件系统写入瞬时流量有硬性上限。探测不使用 release 文件替代实际执行，也未增加架构识别。

4 项针对性检查通过，实际运行的是测试程序自身的辅助子进程，验证正常 stderr 版本输出、非零退出、超时、运行中取消、过量输出、预先取消及临时文件清理，没有运行 Java 或游戏。Clippy 通过；系统版本查询命令仍为独立实现，进程树清理与三端真实 Java 探测仍需继续验收。

## 安装会话与共享状态

继续阅读 [HMCL MojangJavaDownloadTask.java](https://github.com/HMCL-dev/HMCL/blob/main/HMCLCore/src/main/java/org/jackhuang/hmcl/download/java/mojang/MojangJavaDownloadTask.java) 的 `execute` / `postExecute`：先验证 / 下载依赖，依赖成功后才把临时运行时移交至目标目录。该顺序说明单个文件下载完成不能等同于整个运行时安装完成。初批补安装范围的互斥与标记提交，随后进一步接入 RuntimeGeneration，按完整代际发布新运行时。

新增 `RuntimeInstallSession`，按实际组件目录获取 OS 排他下载租约，覆盖清单、文件下载、链接 / 权限处理、运行时探测及完成标记。等待结束才写入 `.installing`，等待被取消不触碰其他任务的状态；失败保留安装中标记，避免发现模块把半成品当作可用运行时。最后检查取消，原子写入清单 SHA-1 对应的 `.complete` 后再移除 `.installing`。无有效清单 SHA-1 不开始写状态，清单也不能覆盖启动器保留路径。

Forge / NeoForge 不同实例仍共享根目录的 `launcher_profiles.json` 及安装器工作目录。新增 `LoaderInstallSession`，复用已有可取消 OS 文件租约，在官方安装器运行期间串行保护根目录；已有配置逐字保留，只有缺失时创建默认配置，格式 / 读取错误不静默覆盖。该锁不跨后续实例资源修复持有，独立游戏根目录不互相阻塞。

初批 7 项本地文件 / 并发检查通过，其中 6 项被沙箱文件原子替换权限阻止，只在沙箱外复查这些临时目录项后通过，没有实际启动安装器、Java 或游戏。

RuntimeGeneration 采用独立 `.pending-<manifest hash>` 目录准备，复用相同清单的中断下载。验证和完成标记通过后，将整个目录移交至新的 generation-*，再原子切换 current.json；不用 HMCL 的清理旧目标目录方式，避免触碰在用 JVM 文件。旧代保留，指针发布失败保留旧记录；已完整移交但未发布的目录也不参与发现，后续孤立目录回收仍待实现。

托管发现只读取当前记录，通用 Java 扫描跳过 `.tui-managed`，旧平铺安装兼容。相同清单的已发布代际须验证声明文件 hash / 大小及链接和 Unix 执行权限，并通过 JavaProbe，才可复用；损坏时建立新代，不直接覆盖旧代。Windows 短路径返回值规范化为一致的物理路径。

新增 5 项相关检查通过，4 项目录原子操作检查在沙箱外复查。包括旧 Windows Java 文件以拒绝写入 / 删除方式打开时仍能发布新代、同大小内容损坏不能通过复用、中断续传、模拟指针写入失败及旧布局发现。未运行真实 JVM、安装器或游戏。真实在用 JVM 的更新、各平台目录耐久性、断电恢复和历史代回收仍需验收，不能把原子指针切换等同于所有崩溃场景已恢复。

## 后续对照方式

后端改动先定位对应源码中的行为、适用版本和失败处理，再核对本项目差异；记录采用规则及未采用原因。优先继续对照 Java 完整版本、Forge / NeoForge 安装及依赖修复、认证刷新和实例隔离。保持服务对象与可替换依赖，不因参考其他启动器而大改 TUI。


## 界面与游戏语言（2026-10-22）

实际阅读本地 HMCL-main：
- HMCL/src/main/java/org/jackhuang/hmcl/util/i18n/SupportedLocale.java：getLocaleByName、默认语言与 ResourceBundle 候选 / 缓存。
- 同目录 I18n.java：当前语言、集中翻译查询和缺失键回退。
- HMCL/src/main/java/org/jackhuang/hmcl/ui/main/SettingsPage.java：语言列表双向绑定，标注重启后生效。
- HMCL/src/main/java/org/jackhuang/hmcl/setting/SettingsManager.java：设置初始化时同时设置 Java Locale 与 I18n Locale。
- HMCL/src/main/java/org/jackhuang/hmcl/game/HMCLGameLauncher.java：generateOptionsTxt、normalizedLanguageTag；1.0 无语言选项，1.1–1.10 要求 zh_CN 等区域大写格式，1.11 起小写；已有 options.txt 或特殊整合包配置时不生成默认文件。

采用语言选择、翻译查询、游戏语言转换分离的设计，缺少所选语言译文时先回退英语，再回退源文案；菜单和带变量的模板先翻译，再插入实例名 / 路径，不替换用户数据。模板现改为受限运行时插值，所有内置目录均通过测试核对占位符。按本项目用户要求增加每次启动同步与 keep 开关、全局 / 实例优先级，切换即时生效；尚未移植 HMCL 的系统区域推断或特殊整合包配置探测。旧版语言另检查实际 JAR / 资源索引，避免写入不存在的语言。

参考链接：[SupportedLocale](https://github.com/HMCL-dev/HMCL/blob/main/HMCL/src/main/java/org/jackhuang/hmcl/util/i18n/SupportedLocale.java)、[I18n](https://github.com/HMCL-dev/HMCL/blob/main/HMCL/src/main/java/org/jackhuang/hmcl/util/i18n/I18n.java)、[HMCLGameLauncher](https://github.com/HMCL-dev/HMCL/blob/main/HMCL/src/main/java/org/jackhuang/hmcl/game/HMCLGameLauncher.java)。

追加核对 HMCL/src/main/resources/assets/lang/I18N*.properties：本地快照共有基础英语与 zh_Hans、zh_Hant、ja、de、es、ru、uk、ar、lzh 共十份资源。项目对应十种语言的独立 JSON 资源现均包含本项目当前目录的 220 条文案，不视为达到 HMCL 全部功能和翻译完整度；缺项按英语回退，选择器展示真实条数。SupportedLocale.isExperimentalSupported 将阿拉伯语列为实验支持；本项目同样保留终端 RTL 未验收说明。文言文低于 1.16 回退简中，来自 normalizedLanguageTag 的版本处理。资源为针对本项目文案重新编写，没有复制 HMCL 的 properties 或翻译内容。

## JVM 架构与目标依赖（2026-10-22）

本批实际读取本地 PCL-main/PCLCS/Java.cs 的 CheckAsync：使用 -XshowSettings:properties -version、15 秒超时与取消，读取属性；当前 PCL 直接拒绝没有 64-bit 标记的 Java。读取 HMCL-main/HMCL/src/main/java/org/jackhuang/hmcl/java/JavaManager.java 的架构兼容判断和 findSuitableJava：区分系统 / 当前架构及 JVM 架构，识别部分 Windows、Linux x86 兼容与 macOS Rosetta；自动候选偏好系统架构，ARM 上极旧游戏有 x86 特例。

本项目借鉴一次属性探测与区分 JVM 目标，解析 os.arch 及可选位数属性，不复制 PCL 的统一禁用 32 位策略。目标规则、native 分类和参数均来自所选 JVM；同主版本优先启动器编译架构，显式路径保留。与 HMCL 仍有差距：未探测独立物理宿主架构 / 仿真能力，没有移植极旧版本强制 x86 或 ARM native 替换。探测成功只表明该 JVM 可运行属性查询，不证明游戏 native ABI 兼容。

## ARM64 native 替换与旧格式库解析（2026-10-22）

本轮实际阅读 HMCL/src/main/java/org/jackhuang/hmcl/util/NativePatcher.java 的 getNatives、patchNative、checkSupportedStatus，及 HMCL/src/main/resources/assets/natives.json 的 linux-arm64、macos-arm64、windows-arm64 数据。核对 LWJGL API / native 配套替换、1.19+ Windows / macOS 官方 ARM 边界、Sodium issue2561 属性，以及 null 表示禁用而非未知的区别。另阅读 HMCLCore 的 Library.isNative / classifier 和 JavaVersionConstraint 中 VANILLA_LINUX_JAVA_8、VANILLA_X86 的边界说明；这些 Java 限制本轮没有全部移植。

阅读 PCL-main/Plain Craft Launcher 2/Modules/Minecraft/ModMinecraft.vb 的 McLibListGet 中 ordinary / natives 分支：没有普通下载 artifact 的 native 条目不需要额外的普通 JAR，库的 null 可选属性会被移除。LibraryResolver 据此修正旧格式 native-only 的多余下载，保留显式 artifact + classifier 组合。

本项目原创 Rust NativePatchService / NativePatchCatalog / NativeArtifact 分离策略与目录数据，以标准依赖形式接入既有下载和解压。assets/native-patches.json 重新组织上述三个目标的 187 项 Maven 替换 / 禁用关系及 83 项去重下载事实（坐标、URL、SHA-1、大小和解压方式），不包含 HMCL Java 代码；来源记录在 assets/NATIVE-PATCHES.md。没有盲目按名称拼接未提供的 ARM 下载地址。

差异：本项目把合并的 artifact / native 分别匹配，禁用 native 时保留显式 Java API；保留用户显式 ARM64 同坐标 native 及其 API。LWJGL API 改变时预置 Sodium 属性，无 Sodium 时不生效，不像 HMCL 先扫描 Mod。未实现 HMCL 自定义 natives 目录、系统 OpenAL / GLFW、其他架构目录、渲染器及 Java agent 补丁。当前仅本地验证目录和接入，没有宣称 ARM 真机或全部真实工件校验完成。

## Forge 旧 / 中间格式与内置库（2026-10-22）

实际阅读本地 HMCLCore/src/main/java/org/jackhuang/hmcl/download/forge/ForgeOldInstallTask.java：读取 install_profile.json 的 install / versionInfo，将 install.filePath 内置 JAR 写到 install.path 的 Maven 位置，返回版本补丁并继续依赖补全。另阅读 ForgeInstallProfile / ForgeInstall 的字段、ForgeInstallTask 的格式判断与 ForgeNewInstallTask 中的 processor 执行段。

实际阅读 PCL-main/Plain Craft Launcher 2/Pages/PageDownload/ModDownloadLib.vb 中 McDownloadForgelikeLoader 的非新版安装分支：Legacy 方式 1 读取 json 字段指向的版本描述并复制 maven/ 支持库；方式 2 提取 install.filePath 到 install.path，补 inheritsFrom 并保存 versionInfo。由此补上“只有 spec: 0、没有 install”的中间格式，避免把同为 1.12.2 的安装包一律当成旧 universal 或新版处理器包。

本项目用原创 Rust ForgeInstallerPlan / DirectForgePlan / InstallerArtifactService 实现结构分流、受限读取与标准依赖发布；额外加入固定安装器来源及条目恢复记录、内容校验和共享文件锁，接入既有安装草稿。没有复制 Java / VB 实现；未复制 HMCL 全部 processor / 游戏补丁模型，也未采用 PCL 的整个归档解压到工作目录方式。

实际只读核对四份官方安装包：Forge 1.7.10-10.13.4.1614-1.7.10（install / versionInfo）、Forge 1.12.2-14.23.5.2859（spec: 0 / maven）、Forge 1.21.1-52.1.20 与 NeoForge 21.1.264（保留官方安装器路径）。前两份本轮从官方 Maven 获取并核对 SHA-1，后两份使用已有本地缓存，检查过程中未执行安装器。真实包结构 / 内置 JAR 解析证据不等于已完成真实安装或启动游戏。

## 安装器 Java 与 JAR 入口（2026-10-22）

实际阅读 PCL-main/Plain Craft Launcher 2/Pages/PageDownload/ModDownloadLib.vb 的 ForgelikeInjector：为其自定义 com.bangbang93.ForgeInstaller 辅助入口选择至少 Java 8u60，并与 JavaWrapper 组合执行。HMCLCore ForgeNewInstallTask 为 processor 读取 JAR Main-Class，使用自身默认 Java 组成独立进程命令。两者都说明安装工具的 Java 选择有独立上下文；本项目直接执行官方安装器，不能无依据照搬 PCL 自定义辅助程序的 8u60 更新号。

新增 RunnableJarSource / JarEntryPointSource 与 InstallerJavaRequirement / InstallerJavaService，以实际入口 class 下限配合游戏要求，并为补装指定满足安装器需求的已知 Java 主版本。只读检查本地 Forge 1.21.1-52.1.20、NeoForge 21.1.264，二者 SimpleInstaller 基础入口均为 Java 8 字节码；游戏仍要求 Java 21。保留这一事实区别，不把入口 Java 8 等同于整个安装链可用 Java 8。

清单主段、Main-Class、续行参考 [JAR 文件规范](https://docs.oracle.com/en/java/javase/17/docs/specs/jar/jar.html)；class 主次版本及预览标记参考 [JVMS 第 4 章](https://docs.oracle.com/javase/specs/jvms/se23/html/jvms-4.html)。实现只读基础入口，不采用构建 JDK 字段推断运行最低版本，也不移植任意外部 Class-Path 或全量 Multi-Release 选择。

另实际检查当前依赖 zip 2.6.1 的 src/read.rs：SharedBuilder.build 把 Vec 插入 IndexMap，file_names 只返回其 keys，同名项在此消失。本轮增加原始中央目录记录计数与解析后名称表长度比较，并以人工构造的重名 ZIP 验证，修正之前计数式“重复条目检查”实际上不成立的问题。此检查现共用于安装描述、内置库恢复和 Java 入口读取。


## Forge 的 Java 更新号与候选筛选（2026-10-22）

实际重新阅读本地 PCL-main/Plain Craft Launcher 2/Modules/Minecraft/ModJava.vb::GetJavaRequirement，重点是 Forge 34.0.0–36.2.25 的 Java 8u320 上限，以及 36.2.26、37.0.79、45.0.21 / 45.0.65 / 45.0.66、47.4.8 的范围边界。同步阅读 HMCLCore/src/main/java/org/jackhuang/hmcl/game/JavaVersionConstraint.java：GAME_JSON、MODDED_JAVA_7 / 8 / 16 / 17、VANILLA_JAVA_8_51 等，核对强制规则与建议规则的差异。

本批只采用 PCL 已明确限定的五段 Forge 上限，并将完整 JavaRequirement 下沉至自动候选选择，保留已有 HMCL LaunchWrapper 上限与 PCL Fabric 限制；不将 HMCL 的所有建议直接提升为硬限制。Forge 身份依靠启用库坐标及实际入口，不依赖用户重命名后的实例 ID。未知后缀、范围外的新 Forge 不自动套用上限。

JavaVersion 对兼容范围只比较发行数字，厂商 build 仍用于同范围的优先排序。修复新更新被拒绝后不再检查其他已安装 Java 的漏洞；“安装建议 Java”也在返回前检查完整约束。PCL 用户自定义范围、自动获取特定旧 JRE、JVM 架构与剩余历史加载器规则尚未移植或验收。


## 缺省 Java 元数据与客户端内置版本（2026-10-22）

实际阅读本地 HMCLCore/src/main/java/org/jackhuang/hmcl/game/GameJavaVersion.java::getMinimumJavaVersion：1.17 → 16、1.18 → 17、1.20.5 → 21、26.1 → 25；GameVersion.java::getVersionFromJson / minecraftVersion 从 JAR version.json 读 id，并去掉“ / ”之后的发行修饰。PCL ModMinecraft.vb::McInstance.JsonVersion 从版本 JAR 读取 version.json；ModJava.vb::GetJavaRequirement 读取 java_version / java_component，并以 2021-05-11、2021-11-16、2024-04-02 作为缺乏有效游戏版本号时的 Java 下限切换点。

本项目新增 GameJavaResolver 与可替换 ClientVersionSource：显式启动元数据优先，缺省时读取实际 resolved.jar_id 对应客户端，继承与重命名不改变 JAR 归属。采用上述版本边界与客户端字段，另读取固定 vanilla Main.class 前 8 字节作为运行时最低版本证据，防止缺失 / 偏低的推断值低于实际字节码要求；此增强不是复制 HMCL 的类常量扫描。对于损坏 / 无法读取的已有 JAR，本项目明确要求修复，区别于 HMCL 部分识别失败后记录警告再尝试其他历史识别方法。

读取有长度和取消检查；已知组件与推断主版本不一致时报错，高版本已知组件可提高缺省要求。没有移植历史 SHA-1 / 类常量映射、全部快照、系统架构选择与所有模组发行版规则。测试仅使用本地 ZIP 与模拟提供者，不是历史 Minecraft 运行验收。


## 公开元数据缓存与备用来源（2026-10-22）

实际读取 HMCLCore/.../task/FetchTask.java 中的 CacheRepository、304 NOT_MODIFIED / useCachedResult、缓存失效后移除记录并重新请求的路径，以及 download/quilt/QuiltInstallTask.java 中对 launchMetaTask 设置缓存仓库的调用。实际读取 PCL ModDownload.vb 的 DlClientListBmclapiMain 和 DlVersionListOrder：要求有效版本列表，检查请求的目标版本是否存在，并控制官方与镜像源顺序。

本项目将网络来源与持久化分为 MetadataSource / MetadataStore，MetadataCache 负责校验后保存和失败回退。业务校验位于每个来源的尝试内部，因此 HTTP 成功但业务数据不可用会继续尝试备用来源。按 PCL 的目标版本存在性检查强化 remote_version，未照搬“列表至少 200 项”的固定数量；增加逐项 ID / URL / SHA-1 和重复 ID 检查，以支持独立小型夹具及未来列表格式。

HMCL 的 HTTP 条件缓存仅作为缓存接入与失效处理参考，未宣称实现其 ETag / 304 全流程。本项目当前为每次联网优先、失败后复用同请求的经验证旧数据，写缓存失败仅记录警告并保留实时结果。缓存包含版本目录、四种加载器的公开准备元数据、安装器 SHA-1 和 Java 组件目录，不包含认证。真实断网安装、条件请求、过期 / 容量管理仍待完成。


## 安装草稿与延迟发布（2026-10-22）

实际读取本地 HMCLCore/src/main/java/org/jackhuang/hmcl/download/game/GameInstallTask.java：下载基础组件返回 GameInstancePatch，不直接发布仓库状态，客户端下载及资源 / 库准备由依赖任务完成；FabricInstallTask.java 同样先形成 patch 并追加 GameLibrariesTask。DefaultDependencyManager.java::updateInstanceAsync 明确要求实例 ID 保持一致，客户端下载先进入 cache / draft，任务失败中止草稿并保留已发布文件，共享缓存保留；它不更新引用的其他实例客户端。

本项目将原版 / Fabric / Quilt 的正式 JSON 写入延迟到准备完成，新增只读候选继承解析和 InstanceDraft。客户端沿用标准 versions/id/id.jar 布局，以保持官方 Forge 安装器的路径兼容；普通提交异常时通过旧客户端备份回滚，下载暂存保留以供重试。引用的父客户端只允许校验复用，父版本须先修复。个人设置通过独立文件与仅内存候选快照保留。

没有复制 HMCL 仓库 draft 实现，也不声称拥有其完整事务特性：本项目尚无持久恢复日志，不保证断电时 JAR 与 JSON 一起提交，尚未处理整个共享缓存 / 玩家目录的事务回滚，以及官方 Forge / NeoForge 安装器的输出回滚。此前对 PCL 下载源排序 / 取消的对照继续保留，本批具体草稿机制主要参考上述 HMCL 符号。


## 提交恢复与读取保护（2026-10-22）

实际重新阅读 HMCLCore/src/main/java/org/jackhuang/hmcl/game/DefaultGameRepositoryDraft.java::commit / applyPrimaryJar / applyManifest 相关路径。它记录 AppliedFile，先应用客户端，再应用配置，异常时用 backupFile 恢复；这是本项目客户端 / 配置提交顺序和普通回滚的行为依据。

本项目在该行为之上新增自己的持久恢复协议，不宣称 HMCL 使用相同日志格式：固定实例目录内的 .tui-mc-install-journal.json 保存 schema、实例 ID、暂存指纹与文件指纹，旧配置放在所属暂存目录备份。按实际文件状态判断提交已完成或应回滚，恢复可重复执行，遇到未知编辑则保留现场。没有借用 HMCL / PCL 的发行标识、凭证或代码实现。

版本解析改为每个节点上锁后再读。启动对父版本共享读取，对需恢复的父版本先取得独占锁恢复并重新取得共享锁；显式 jar 引用的归属也受保护。验证包含辅助 Rust 子进程退出和锁释放，不包含真实断电或 Minecraft 运行，不能将进程退出恢复等同于所有存储故障均可恢复。

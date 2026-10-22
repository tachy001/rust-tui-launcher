# 后端职责与服务接口

后续开发以服务对象封装业务状态与策略，trait 隔离外部能力，`Backend` 作为 TUI / CLI 的兼容入口。保留已有功能，逐步拆解，不通过全面改写前端推动重构。

## 当前拆分

| 对象 | 负责的行为 | 不承担的工作 |
| --- | --- | --- |
| `Backend` | 接收启动 / 安装请求、协调服务、发送现有事件 | 不再实现 Java 安装失败后的选择策略 |
| `InstanceConfigResolver` | 合并全局、随实例保存和本地覆盖设置，保留强制隔离；传播配置读取错误 | 不创建游戏目录、不隐式覆盖损坏文件 |
| `InstanceConfigSource` / `FileInstanceConfigSource` | 可替换的实例设置与隔离标记读取；区分文件缺失与读取 / 格式失败 | 不将解析失败当作没有设置 |
| `ProcessMonitor` | 游戏与安装器共用退出、取消和输出跟随策略；日志读取失败降级为警告 | 不启动命令、不决定游戏异常诊断 |
| `ProcessHandle` / `ManagedProcess` | 进程状态查询、终止和等待回收，RAII 保护提前返回 | 不保证清理第三方程序派生的整个进程树 |
| `ProcessOutput` / `LogTail` | 分段读取日志，进程结束后补齐未换行的末行 | 不把日志读取失败等同于进程失败 |
| `AccountService` | 协调登录、刷新、选择、离线添加与移除；保存失败时保留索引并恢复游戏凭证 | 不访问微软协议端点、不向界面传递令牌 |
| `AccountIndexStore` / `FileAccountIndex` | 提供索引修改锁与按账户区分的刷新锁、读取与原子保存索引 | 不存储令牌、不执行认证请求 |
| `AccountMutationLease` / `AccountIndexLock` | 在索引读取、凭证变更、索引保存及回滚期间持有 OS 排他锁 | 不删除锁文件，不协调不同工作目录的账户索引 |
| `RefreshSession` | 持有单账户的网络刷新会话，保管 Zeroizing 凭证快照，校验账户代际及当前凭证后保存 / 提交 | 不跨网络请求持有全局账户锁，不把过期结果写回账户 |
| `CredentialStore` / `SystemCredentials` | 可替换的凭证读取、保存与删除；生产实现使用系统安全存储 | 不将令牌回退到普通配置文件 |
| `JavaRuntimeService` | 显式 Java、建议版本与安装失败回退策略；保持取消状态 | 不直接执行 Java、不访问运行时下载服务 |
| `JavaVersion` | 解析并保留完整版本、更新号、预览状态与构建号，提供数值比较；JavaRuntime 由此派生主版本 | 不运行 Java，不推测厂商间更新号等价关系 |
| `JavaProbe` | 运行受超时、取消与输出预算约束的 Java 版本 / 架构属性探测，持有直接子进程并清理私有捕获文件 | 不使用游戏进程的无限等待策略，不负责整个派生进程树 |
| `JavaArchitecture` / `LaunchPlatform` | 解析 JVM 架构与位数，统一依赖、加载器限制与参数的目标平台 | 不由启动器位数猜 JVM、不证明 native 二进制 ABI 可用 |
| `JavaRequirement` | 从启动元数据提取建议主版本与已知加载器强制上限，校验候选运行时 | 不按实例名称猜测版本、不宣称覆盖 Java 更新号兼容性 |
| `JavaRuntimeSource` | 提供本地选择、建议版本安装和任务状态能力 | 不决定是否允许回退 |
| `RuntimeResolution` | 封装选择结果和可展示警告 | 不向界面直接打印 |
| `ArgumentParser` | 按系统规则将自定义 JVM / 游戏参数解析为 argv，校验与启动共用 | 不执行 shell、不展开环境变量、不解析旧版 Minecraft 元数据参数 |
| `LaunchClasspath` | 按首次出现顺序去重库路径，客户端最后加入，校验路径可表示性与分隔符 | 不下载、校验或提取 native，不宣称实现 OptiFine 混装 |
| `BootstrapCompatibility` | 按 BootstrapLauncher 版本修复 JVM ignoreList，加入真实客户端并防止目录子串误匹配 | 不改写游戏参数、未知限定符版本或磁盘元数据 |
| `InstanceMutation` | 通过 RAII 持有目标与依赖实例的写锁，在完整安装 / 修复流程内复用同一会话 | 不替代下载缓存或其他共享目录的跨实例锁 |
| `RuntimeInstallSession` | 保护准备目录的清单和安装标记，验证后原子发布完成标记 | 不决定当前代际，也不删除旧运行时 |
| `RuntimeGeneration` | 同组件 / 平台仓库互斥、私有目录续传、文件校验复用、完整目录移交与 current.json 原子切换 | 不覆盖旧代文件，不自动回收仍可能被使用的历史代际 |
| `LoaderInstallSession` | 持有游戏根目录的加载器安装锁，初始化缺失公共配置并验证现有文件 | 不覆盖损坏配置，不替代目标实例锁或后续资源修复 |
| `DownloadPlan` | 每个目标路径只生成一个任务；合并校验条件和备用 URL；拒绝冲突 | 不发起网络请求或写入文件 |
| `DownloadLease` | 对同一下载目标持有操作系统文件锁，覆盖断点、镜像回退、重试与提交；等待可取消 | 不替代实例运行锁或全部文件系统写入协调 |
| `LibraryResolver` | 按目标平台解析现代 / 旧式 Maven 库与 native classifier，补全可推导的文件路径 | 不访问网络、不探测宿主平台以外的目标、不执行解压 |
| `PlatformRuleEvaluator` / `OsVersionSource` | 注入平台条件判断库和参数规则，支持架构正则与别名；生产版本提供者按需缓存系统查询 | 不推测跨架构 Java 环境，不将系统版本查询失败当作空版本 |
| `NativeDirectory` | 持有单次启动的独立 native 目录，按排除规则解压、检查任务状态，失败后拒绝提供路径并在释放时清理 | 不复用旧文件、不清理其他会话；不保证 OS 强制终止后的回收 |
| `ModDependencyResolver` | 对平台声明及可插入的包内约束进行有界回溯，保持固定版本身份并缓存候选列表 | 不直接读取或下载 JAR |
| `ModCatalog` | 按项目或固定版本提供候选，检查任务取消状态 | 不决定依赖搜索顺序或替换已安装文件 |
| `ModPlanValidator` / `FabricPlanValidator` | 检查候选组合与已保留 Mod 的 Fabric 包内范围；区分兼容性拒绝与 I/O 错误 | 不替换已安装文件，不放宽固定版本 |
| `ModArtifactSource` / `StagedModArtifacts` | 校验并复用本地文件，按版本缓存候选 JAR，缺失时通过 Downloader 下载 | 不决定依赖版本选择 |
| `ModInstallState` | 按已管理项目的原文件确定启用 / 禁用状态，拒绝双副本与相反状态文件冲突 | 不因版本更新自动启用已禁用 Mod |
| `ModInstallTransaction` | 独立备份、SHA-1 校验后的替换、安装记录与整合包记录更新、失败回滚 | 不进行网络查询或候选搜索 |
| `Downloader` | 执行经过校验的计划，处理并发、重试、断点与文件提交 | 不静默忽略重复项的校验约束 |

`JavaRuntimeSource` 的生产实现暂由 `Backend` 适配已有 Java 下载和探测模块；本地测试使用替代提供者，不启动 JVM 或请求外部服务。现有其他模块仍按需逐步迁移，不代表已全部面向对象重构完成。

## 业务约束

- `Config::effective` 返回 `Result<Config>`；后端目录操作必须先完成解析，失败时不得退回全局目录。前端展示读取错误，保存也传播该错误。只有文件缺失允许继承，损坏 JSON、非对象版本配置与非布尔 `tuiMcIsolated` 必须明确报错。
- 设置按全局、随实例保存的 `tui-mc-instance.json`、`launcher.json` 的实例覆盖依次合并；空字符串与 false 均为显式覆盖。版本配置要求强制隔离时最终保持隔离。全局配置保存前也校验全部本地实例覆盖，避免保存无效内存、参数、Java 或服务器配置。
- 游戏与安装器在创建进程前再次检查任务状态，并预先打开日志读取句柄。实时日志读取失败只警告一次并停止跟随，继续监控退出 / 取消；失败退出仍交给调用者诊断，不改报为成功。
- 进程已退出时优先返回真实状态；运行中的取消先终止并等待回收，再排空剩余日志。运行阶段不通过任务暂停阻塞进程监控。终止与自然退出竞争时复查状态，避免误报终止失败。
- `ManagedProcess` 保留错误返回时清理直接子进程的责任；实际 Windows 辅助子进程检查不代表整个进程树或其他系统已验证。
- 账户选择、离线添加和移除只在持久化成功后更新调用者状态。读取失败、损坏 JSON、重复 / 空 ID 与不存在的当前账户不得作为空索引继续覆盖。
- 登录先保留服务器返回的最新刷新令牌，再保存游戏令牌与索引；后续失败恢复原游戏令牌，刷新令牌不回退。刷新保持原选择，拒绝身份变化和已被移除的账户。
- 移除账户前读取两种凭证快照；删除或索引保存失败时尝试恢复全部快照，单份恢复失败不能阻止另一份恢复，恢复不完整时提示重新登录。令牌不写入索引或业务错误信息。
- 对同一账户索引的修改必须先获取 `.tui-mc/accounts.lock`，读取、凭证变更、索引保存和失败回滚期间持有该锁；冲突时明确失败，避免等待阻塞 TUI。持久锁文件不删除，实际锁随句柄释放。刷新令牌的提前保存也经服务加锁，拒绝账户已移除或 OAuth 客户端已变更的旧结果。
- `RefreshSession` 从读取刷新令牌前到 Minecraft 授权完成持有该账户的独立锁，其他账户刷新及本地账户选择仍可进行。新登录 / 成功刷新从索引单调 revision 分配账户 generation；删除不回退 revision，删除后重加也不会复用旧代际。提前保存 OAuth 结果与提交游戏凭证都复查 generation、身份和客户端。
- 刷新会话还比较当前安全存储与自己的凭证快照；新登录失败但已保留刷新令牌时，也不会被旧会话覆盖。旧格式的 revision / generation 默认 0；计数溢出时在凭证写入前失败。
- 文件索引与系统凭证存储不具有跨存储原子提交：进程崩溃恢复与真实系统凭证往返仍待验收。不同工作目录的索引不共享锁 / revision，跨目录共享系统凭证仍需协调，不能据此宣称所有账户竞争已消除。
- 显式 Java 不自动替换；版本过低时失败。
- Java 发现、手动路径和下载后验证共用 JavaProbe，运行阶段最多等待 15 秒；取消检查不被暂停阻塞。轮询合计输出量，超过 1 MiB 便停止；最终内存读取也有限制。输出使用私有临时文件，避免等待被派生进程继承的管道 EOF，直接子进程先结束 / 回收再清理捕获目录。文件系统或进程创建本身的阻塞不属于该运行阶段计时。
- native 文件在单次启动私有目录中准备，不直接覆盖旧目录；解压中途失败后不能用部分文件继续启动。目录声明早于游戏进程，RAII 释放顺序保证先回收直接子进程再尝试清理。清理前复查绝对父目录与子目录归属，校验失败则保留目录。
- Java 选择、安装和失败回退都必须满足 JavaRequirement；旧 LaunchWrapper 的 Java 8 上限与旧 Fabric Loader 的 Java 24 上限不能因下载失败而绕过。约束只依据当前平台启用的库与对应主类；本地选择及安装同时失败时保留两份上下文。源码依据和未覆盖范围见 [SOURCE-REFERENCES.md](SOURCE-REFERENCES.md)。
- 自动模式优先使用本地建议版本，否则尝试安装建议版本；安装失败时仅可回退到满足最低版本的已有运行时，并返回警告。
- 任务取消后不得把安装失败变为成功回退；安装结果版本不符不得作为建议运行时接受。
- 同一路径重复下载合并更强的 SHA-1 / 大小条件；不同 hash 或大小立即报错。同内容镜像必须具备相同 SHA-1，备用地址保留到执行阶段。
- `Downloader` 在创建 / 修改 `.part` 前获取 `DownloadLease`，等待后重新检查目标缓存。相同物理父目录使用同一个锁目录，Windows / macOS 对名称折叠大小写；不同文件使用不同锁。锁文件保留，避免删除仍被其他进程持有的锁造成互斥失效，实际锁随句柄关闭释放。
- 锁元数据放在目标父目录的 `.tui-mc-download-locks` 中，实例数据导出、复制和存档备份不带出该目录；整合包导入不能覆盖该保留目录。
- 旧式 native 库通过 classifier 坐标独立解析，不能复用普通 JAR 的 hash / 大小；现代 classifier 缺失仍明确报错，不猜测下载内容。可为 Windows / Linux / macOS 注入目标平台进行本地检查，这不等同于各平台实际运行验收。
- 原版重装在写元数据前获取目标锁；修复在下载客户端 / 依赖前保护继承链与客户端所属实例。读取继承关系后加锁并再次解析，关系变化则明确失败，避免按旧关系写入。
- Java 新安装在 `runtime/.tui-managed/<组件>/<平台>/.pending-<清单hash>/` 中完成；RuntimeGeneration 持有组件仓库锁，RuntimeInstallSession 保护准备目录的文件与标记。运行时探测通过后完成标记提交，目录移交为独立 generation-*，再原子切换 current.json，旧运行时和旧记录在发布失败时保持不变。
- 通用扫描不进入托管仓库；托管发现只读取有效 current.json，不暴露准备目录、旧代或尚未发布的完整目录。旧版平铺安装仍可发现。清单相同的当前代经全部声明文件的 SHA-1 / 大小、链接 / 执行权限检查及实际 Java 探测后可复用；校验失败建立新代，不能原地修复可能在用的文件。清单不能覆盖启动器保留路径。
- Forge / NeoForge 在选择好运行时后获取根目录 LoaderInstallSession，锁覆盖公共配置初始化、官方安装器执行及生成元数据检查；释放后再执行只涉及当前实例的修复。公共配置只有确实缺失才初始化，读取或格式失败时不覆盖原文件。
- Fabric / Quilt 配置写入、Forge / NeoForge 安装器执行与后续修复复用目标会话。整合包与在线 Mod 修复要求传入匹配实例的会话，不再各自重复获取已持有的锁。
- 认证、实例、加载器与启动流程的后续拆分沿用相同原则：业务策略可独立验证，保持现有 TUI / CLI 兼容。
- Mod 平台依赖规划优先正式版和较新发布时间，后续固定版本冲突可回溯。项目 + 版本不兼容约束同时指定时必须同时匹配；替换计划中的旧记录不作为最终启用冲突项。搜索最多 4096 步、256 个项目及 2000 个待处理请求，查询失败直接返回错误，不能伪装成无兼容版本。
- Fabric 安装可验证最多 32 组完整候选组合；包内范围冲突会回溯，下载、读取、JAR 损坏与取消错误直接中止。候选缓存位于实例 `tui-mc-mod-downloads/candidates/<版本ID>/`，只将选中组合复制到事务暂存区。最终替换前仍执行完整预检。
- 更新保留原项目的启停状态，即使版本更换文件名；禁用候选不能满足启用组合的 Fabric 依赖。安装记录与整合包记录继续保存标准文件名，由实际 `.disabled` 文件表达状态。替换失败恢复旧文件及两份元数据快照，回滚错误必须报告；已有备份目录不可覆盖。


## 语言职责

localization::UiLanguage 解析全局界面语言，assets/lang/*.json 通过 include_str! 编入程序，OnceLock 缓存语言包；查询顺序为所选语言 → 英语 → 源文案。FormatArguments 对命名 / 位置参数进行受限插值，保留备用格式 #，错误模板回退且不处理变量自身的内容；测试逐项校验所有语言的占位符与原文相同。UI 使用线程局部语言状态，后台任务在创建时显式继承界面语言。语言选择器从 UiLanguage::ALL 获取可选语言、原生名称和译文覆盖数，不自行处理游戏语言策略。

GameLanguage 处理 launcher / keep / 显式游戏代码及新旧版本格式，GameOptionsService 独立处理 options.txt 的保留式编辑和原子保存。Backend 在启动前校验完成后、创建子进程前执行同步；同游戏目录的 options.txt 租约持续到进程结束，覆盖隔离 / 共享目录。实例配置合并与克隆继续经过原有服务，前端不承担语言同步策略。默认跟随启动器，显式 keep 不读取或改写 options.txt。


## Java 兼容候选池

JavaRequirement 组合主版本和可选完整发行版本上限，JavaCeiling 负责限制比较与诊断。JavaRuntimeSource.select 接收完整需求；Backend 负责有副作用的发现 / 探测，JavaRequirement.choose 只负责候选过滤，再复用 java::choose 的版本排序。显式路径保留原选择并校验，失败不擅自替换。JavaRuntimeService 继续协调本机候选、建议组件安装和兼容回退；直接安装建议 Java 的入口也验证完整需求。兼容边界比较发行数字，候选优先级才比较 preview / build，避免把同更新号的厂商构建误判越界。


## 游戏版本到 Java 需求

GameJavaResolver 管理来源优先级和回退，ClientVersionSource 为可替换的只读依赖，JarClientVersionSource 负责受限读取 version.json 与 Main.class 头。GameJavaPlan 提供 JavaRequirement、可选组件和来源说明；启动和 Java 安装共用。显式启动元数据优先，不依赖 JAR 可用；缺省模式会传播文件 / 格式错误和取消，且沿 resolved.jar_id 读取实际客户端归属，使用 chain.first 的原版身份作为回退。JavaRequirement 继续负责加载器约束和候选范围交集，解析服务不承担下载或进程启动。


## 公开元数据缓存

MetadataCache 协调候选来源、同一解析器的实时 / 缓存校验、非致命缓存写入失败和取消。MetadataSource 提供候选地址和有大小上限的 HTTP 字节读取；MetadataStore 提供读取 / 保存，可用内存提供者验证策略。FileMetadataStore 使用 URL 的 SHA-1 文件名及包含 URL 的格式头、原子写入和有界读取，并兼容旧 versions.json。Backend.cached_metadata 只作为显式公开元数据入口，认证客户端和 get_json 的其他用途保持独立。各业务模块在提交缓存前检查其目录 / 请求父版本 / Maven 坐标 / 校验格式，不把“HTTP 成功”当作“可使用元数据”。当前没有 TTL / 条件请求 / 缓存清理；网络调用期间的取消仍受阻塞 IO 超时限制。


## 实例安装草稿

InstanceMutation 管理目标 / 继承 / 客户端归属的锁，resolve_candidate 只用内存候选覆盖目标配置并在获得父锁后重读依赖。InstanceDraft 管理确定性暂存路径、客户端缓存复用、自有客户端准备、配置发布和普通失败回滚；ProfilePublisher 为可替换的原子配置 IO，错误须保留之前的正式配置。Backend.install_profile_with_session 协调准备和提交，原版 / Fabric / Quilt 共用；普通 repair 继续修复现有配置。

Config.with_profile 提供仅内存候选快照，FileInstanceConfigSource 在对应目标的安装上下文使用它；其他实例仍读取自己的文件。嵌套的 effective 调用保留快照，因此隔离、Mod 和整合包修复目录一致，active_profile 被 serde 跳过。暂存与缓存不是用户可运行实例，直到正式 JSON 发布才可被 installed 发现。客户端仍放在标准路径，没有另起脚本或改变界面。

持久崩溃恢复、草稿回收、跨卷提交、共享依赖整体回滚及官方安装器输出事务尚未完成。遗留客户端备份不会被下一次安装覆盖，恢复 / 清理失败会明确报告路径。


## 安装提交恢复协议

InstanceRecovery 管理有界恢复记录读取、文件指纹、旧配置备份及可重复恢复；InstanceDraft 在自有客户端切换前写入记录，普通异常调用同一回滚协议，成功后检查发布结果再清理。复用已有客户端时只原子更新 JSON，不需要跨文件日志。正式文件与记录不一致时不推测用户意图，停止并保留备份。记录清理失败但已确认活动文件一致时返回提示；写入会话不允许在旧记录上叠加新事务。

InstanceMutation 在取得每个独占锁后执行恢复，InstanceLaunchSession 使目标恢复先于配置 / 隔离设置读取，父版本和 jar 归属保持共享锁直到游戏进程退出。version::resolve_guarded 在读每份元数据之前调用锁定 / 恢复回调，防止先解析到恢复前的继承关系。读取会话在父版本混合状态下先放开共享锁、独占恢复、再重新共享锁并复查；无法取得锁则明确失败而非使用不稳定数据。

这套协议覆盖本启动器草稿的客户端与版本 JSON。共享依赖、玩家文件、官方安装器输出和无记录的旧备份不在自动恢复范围。数据 / 记录文件有文件同步，但尚未验证真实断电与所有平台目录项耐久性，仍不能承诺断电安全。

## JVM 架构与启动依赖

JavaProbe 一次执行 -XshowSettings:properties -version，版本仍由 JavaVersion 解析，JavaArchitecture 从 os.arch 识别 x86、x86_64、ARM、ARM64、RISC-V64、PPC64 / LE、S390x 及别名。可选 sun.arch.data.model / com.ibm.vm.bitmode 必须与架构一致；缺失 os.arch、未知架构或矛盾属性明确失败，不回退猜测。识别某架构不表示提供了对应平台的运行时下载或 native 补丁。

JavaRuntime 保存 architecture；同一主版本 / 稳定性内优先与启动器架构一致的候选，再按完整版本排序，仍允许已探测成功的其他架构 JVM。托管运行时发布和复用还要求其架构与请求的目录平台一致。当前仍以启动器编译目标作为自动下载平台和本地架构优先级，没有另外探测物理宿主架构。

LaunchPlatform 创建以 JVM 架构为准的 PlatformRuleEvaluator 与按 JVM 位数替换 ${arch} 的 LibraryResolver。profile 在内存中筛选库并移除已评估规则，确保 Bootstrap 等下游不会再按启动器架构反向筛掉选中的库，不修改磁盘配置。JavaRequirement 保存原始配置，校验每个候选时重新生成其目标库视图，避免条件库的 Java 上限误用于其他架构；配置预检只在全部已知目标都无效时因平台限制失败。JVM 与游戏参数共用目标规则，保留动态 features 判断。

Backend.prepare_launch_libraries 在任何游戏进程创建前集中解析所选目标依赖，复用 Downloader 的校验、缓存、共享锁、重试与取消；缺失 / 损坏项可补下载。随后才解压 native 与构建类路径，本次 native 目录名称包含实际 JVM 架构。普通安装 / 修复仍预取启动器平台依赖，切换 JVM 目标后在启动前补齐所需变体。

这层只保证使用元数据表达的目标规则与分类，不校验 PE / ELF / Mach-O 内部架构、不替换缺少 ARM 变体的旧版 LWJGL、不覆盖第三方 native Mod。32 位内存上限、Rosetta / Windows ARM 仿真及跨平台真实运行仍需完善或验收，不应据此宣称所有跨架构游戏已可启动。

## ARM 原生依赖兼容服务

NativePatchService 位于 LaunchPlatform 的库筛选之后、启动依赖预检之前。NativePatchCatalog 为可替换的依赖目录；EmbeddedNativeCatalog 加载并校验内置版本化目录，NativeArtifact 生成标准依赖元数据并强制固定 SHA-1 / 大小。NativePatchPlan 返回内存配置、操作提示和 LWJGL API 版本改变标记，不访问网络 / 文件系统，不修改正式配置。

当前目录对应 HMCL 提供的 Windows、macOS、Linux ARM64 已知坐标：旧 LWJGL 3 的 API 和 native 配套转换，Linux 部分现代 native 改为 ARM64 分类，macOS 特定 GLFW 使用对应兼容实现，LWJGL 2 仍为需解压的 native。合并条目的普通 JAR 与 native 独立匹配；禁用不支持的语音 / 手柄 native 时保留显式 Java API。未知条目保留并给出 native 缺少替换的提示。显式 ARM64 同坐标 / 版本 native 及其 API 保留，避免覆盖整合包自己的配套库；处理可重复，不再次替换生成的目标条目。

Config.patch_natives 默认为 true，InstanceSettings.patch_natives 可覆盖并在克隆时保留；CLI 临时启停覆盖实例配置但不保存。Windows / macOS 仅以继承链首个基础条目的正式版本判断 1.19+ 官方 ARM 依赖边界，不使用子实例名称。版本无法识别时仍只对目录中精确匹配的坐标处理；不是按未知版本猜测下载地址。

Backend 将计划交给既有 prepare_launch_libraries / Downloader，再解压或加入类路径。LWJGL API 版本改变时，在元数据及用户 JVM 参数之前添加 Sodium issue2561 属性；该属性只由 Sodium 读取，用户后置参数可覆盖。LibraryResolver 同时修正无 downloads 的 native-only 旧格式，不再凭空补一个普通 Maven JAR，显式 artifact 才同时保留；可选 null 字段按缺失处理。

这补上前节缺少 ARM 兼容目录的一部分，但不证明原生库二进制或 ABI 已兼容。没有运行真实 ARM JVM / 游戏，没有验证目录内所有真实文件的可达性与加载，其他架构、系统原生库选择和自定义渲染器也未移植。

## Forge 安装包分流与内置库恢复

ForgeInstallerPlan 按包内结构生成安装计划：有 install / versionInfo 的旧格式读取内置 universal JAR；无处理器的 spec: 0 格式读取声明的版本 JSON 和所需 maven/ JAR；其他结构继续使用官方安装器进程。分流不根据游戏主版本猜测。元数据限制 1 MiB，单个内置 JAR 128 MiB，spec: 0 内置库合计 256 MiB / 1024 项；重复 / 缺失条目、越界路径、错误父版本和核心坐标、无效 JAR 明确失败。历史 minecraftforge 坐标可不含 Minecraft 版本前缀。需要替换 / 合并客户端的旧描述不伪装成直接 universal 安装。

DirectForgePlan 只构造内存配置：补父版本继承、保持 tweaker 参数，以内置内容实际 SHA-1 / 大小规范下载描述，并附 tuiMcEmbedded（固定来源安装包 URL / SHA-1 与包内条目）。Backend.install_forge 将可直接安装的计划交给既有 InstanceMutation / InstanceDraft，不探测或启动 Java；正式 JSON 在依赖完成后发布。新版 Forge / NeoForge 的处理器仍由官方安装器执行，没有在本轮移植全部 processor 引擎。

InstallerArchiveSource 分离安装包取得能力，Backend 提供下载器实现；InstallerArtifactService 管理校验、缓存、包内读取与原子发布。安装、repair 和启动预检在处理依赖前使用它：已验证的库不访问来源；缺失或损坏时取得固定 SHA-1 的安装包，再核对条目内容和 JAR，取得共享目标下载锁后重查并原子替换。坏安装包 / 条目不会覆盖旧库；恢复失败中止安装，草稿不发布新配置。

安装包按 SHA-1 缓存到 tui-mc-installers/<sha1>.jar，之前人类可读文件名的缓存经校验可迁移复用，日志文件名保持可读。内置库引用指向公开下载源，不含用户目录或凭证；来源缓存被删除后，可在网络可用时重新获取。该协议保护内置库发布和版本配置发布，不整体回滚共享依赖、官方安装器输出或 JVM 处理器副作用。

## 安装器 Java 选择

RunnableJarSource 隔离 JAR 入口检查；JarEntryPointSource 读取清单主段的 Main-Class，按 CR / LF / CRLF 和字节续行展开，再检查基础入口 class 的头部。只读取最多 64 KiB 清单前缀，主段结束后的签名 / 单文件属性不参与入口选择；不会把 Build-Jdk-Spec、Created-By 或 META-INF/versions 下的替代 class 当成基础 Java 下限。缺少或重复 Main-Class、非法类名、缺失入口、无效字节码和当前未启用的预览入口明确失败。这里只证明入口字节码下限，不验证全部 JVM 指令 / 链接或外部 Class-Path。

InstallerJavaRequirement 组合安装器入口下限和 GameJavaResolver 的游戏要求，至少 Java 8；游戏 Loader 的上限不套用到安装器。InstallerRuntimeSource 提供本机选择 / 指定主版本补装，InstallerJavaService 协调选择、取消和结果复查。本机有满足下限的 Java 直接复用，不强制为了得到某个推荐主版本额外下载；手动路径失败不替换。补装按安装需求选择已知组件 8 / 17 / 21 / 25，精确下限为 16 时保留 Java 16 组件；未知更高下限可使用本机 Java，但不假装已有下载组件。安装器要求高于游戏时，补装不会再误用游戏组件。

Backend.install_forge 在直接安装分支之后读取入口，日志同时显示安装器和游戏下限，再运行官方安装器。旧 / 中间格式仍不创建 Java 进程。补装返回的主版本必须与请求一致且满足下限，取消优先于安装返回值；双重失败保留本机与下载两部分原因。尚未推导完整依赖 / processor 字节码、Java 更新号 / 厂商约束或安装器的所有 Java 上限。

## 安装器 ZIP 目录检查

installer_archive::open 为入口检查、安装描述解析及内置库恢复共用。当前 zip 2.6.1 在解析后将文件存入按解码名称索引的 Map，同名项被覆盖，file_names().count 无法证明无重复。新增从已解析 central_directory_start 定位、逐条读取原始中央目录头并跳过变长字段，以原始条目数对比名称表长度；不重新实现 ZIP 解压或 ZIP64 偏移定位，不信任被合并后的名称计数。检查有取消、偏移 / 文件边界和十万条限制，不一致则拒绝整个安装包。自制重名 ZIP 证明原依赖库会只暴露一个名称，而新检查会拒绝；此修正补齐上一节“重复条目拒绝”的实际验证边界。

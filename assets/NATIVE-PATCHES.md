# ARM 原生库兼容目录

`native-patches.json` 编入可执行文件，不随发布包单独分发。当前格式版本为 1，包含 Linux、Windows、macOS ARM64 的 187 项源坐标处理及去重后的 83 项下载描述。运行逻辑位于 `src/native_patch_service.rs`，不会执行 Java、下载文件或改写实例配置。

数据来源为用户提供的 HMCL 本地快照 `HMCL/src/main/resources/assets/natives.json`。目录重新组织了其中对应三个目标的 Maven 坐标、下载地址、SHA-1、大小及替换 / 禁用关系，不包含 HMCL Java 实现或 JAR 文件。行为参考 `NativePatcher.patchNative`；来源与差异详见 `docs/SOURCE-REFERENCES.md`。新依赖自身的许可信息仍由相应项目及分发包提供。

`targets` 按 OS 与 JVM 架构选择：`linux-aarch64`、`windows-aarch64`、`osx-aarch64`。值为目标坐标，`null` 表示该 native 在此平台不可用而从启动依赖禁用；不存在的键表示保持原条目。旧格式库的普通 JAR 与 `:natives` 分开匹配，后者是本目录的逻辑变体标识；删除 native 不删除显式声明的 Java API JAR。`artifacts` 按目标坐标去重，保存固定下载信息，路径由 Maven 坐标生成。`extracted: true` 表示解压至本次启动目录，其他 JAR 加入类路径，让对应 Java 库加载其资源。

所有下载描述必须有有效 HTTPS URL、40 位 SHA-1、非零大小；使用同一下载器进行实际文件校验。目录加载校验引用完整性、格式、坐标与下载字段。测试检查这些元数据关系并使用自建 ZIP 验证接入，不表示已下载验证全部 83 个真实文件或完成 ARM 游戏运行。

启用 / 禁用和版本边界：

- 默认启用，`launcher.json` 的全局 `patch_natives` 和实例覆盖可设为 `false`；命令行 `--no-native-patches` 单次关闭，`--native-patches` 单次启用。
- 仅 ARM64 应用当前目录。Windows / macOS 原版基础链首项为可识别的 1.19 及以上正式版时保留官方依赖；不根据可编辑的子实例名判定此边界。
- 已显式提供 ARM64 native 的同坐标 / 版本库及其对应 API 保留。未知依赖保持原样，已知库缺少 native 替换时记录提示。
- LWJGL API 版本改变时，在用户自定义 JVM 参数之前加入 `-Dsodium.checks.issue2561=false`，兼容 Sodium 对原始 LWJGL 版本的检查；没有 Sodium 时该属性不生效。用户可用后置自定义参数显式覆盖。
- 禁用旧版语音或手柄 native 可能使这些附加功能不可用，日志会记录；不能把禁用视为提供了对应 ARM 功能。

目录只是兼容策略的一部分。PE / ELF / Mach-O 内部架构、ABI、第三方 native Mod、自定义渲染器与系统 OpenAL / GLFW 等尚待完善；macOS、Linux、Windows ARM64 真实游戏运行仍需验收。更新目录时保留固定校验信息，并核对 Java API 与 native 是否需要配套更新，不要仅凭文件名构造未验证的下载地址。

# Modrinth 安装验收与依赖修正

## 2026-10-21：Fabric 包内约束参与回溯

已将 Fabric 包内范围接入 ModDependencyResolver 的候选验证。独立的 ModArtifactSource 提供已校验的候选文件，FabricPlanValidator 将兼容性冲突反馈给搜索；下载 / 读取 / ZIP 错误直接终止，固定版本不会被悄悄替换。候选先缓存，不直接修改已安装 Mod，最多验证 32 组组合。

4 项服务检查覆盖本地 ZIP 中的 Iris / Sodium 范围回溯、固定不兼容版本失败、坏 JAR 中止、下载失败不继续回退和已禁用文件的只读复用；原有范围预检检查保持通过。

额外只读检查复用工作区已有的真实 Iris 1.8.8、Sodium 0.6.13 与备份中的 Sodium 0.8.13 JAR。采用本地候选目录读取真实的 fabric.mod.json 和嵌套 JAR，解析结果选择 `0.6.13+mc1.21.1`。该检查没有访问网络、没有重新安装文件或启动游戏。平台候选 ID 在此测试中使用本地标识，不把此结果当作新的真实 Modrinth API 安装验收。

仍未证明：新解析流程的完整联网安装、任意 Mod 组合、Forge / Quilt / NeoForge 包内依赖求解、该组合的实际图形运行。以下为先前真实联网安装记录。

日期：2026-10-19。环境：Windows，Minecraft 1.21.1，Fabric Loader 0.19.5。

真实安装入口使用已有验收实例，没有启动游戏。

```powershell
.\target\release\tui-mc.exe mods fabric-loader-0.19.5-1.21.1 download iris --game-dir .tools/smoke-game
```

下载与 SHA-1 校验成功，必需依赖解析找到 Sodium。进一步读取 JAR 元数据发现：Iris 1.8.8 声明 `sodium: ["0.6.x"]`，平台未固定依赖版本，旧逻辑选中了同样支持 Minecraft 1.21.1 的 Sodium 0.8.13。因此，首次下载成功不能视为组合兼容成功。

0.12.0 补上 Fabric 包内依赖检查，在在线安装替换文件之前与游戏启动之前执行。与此真实问题对应的回归检查通过；该版本依赖比较边界修正后，只复查了相关比较项，没有重跑整个检查集。

随后只进行一次明确版本修正：

```powershell
.\target\release\tui-mc.exe mods fabric-loader-0.19.5-1.21.1 download sodium u1OEbNKx --game-dir .tools/smoke-game
```

入口退出码 0，包内依赖预检通过并提交安装，旧 Sodium 文件由现有事务备份机制保存。

最终管理记录：

| 项目 | 平台版本 ID | 文件 | SHA-1 |
| --- | --- | --- | --- |
| Iris | `zsoi0dso` | `iris-fabric-1.8.8+mc1.21.1.jar` | `4e0df4011503b4b36ba69877ea7247c1a47fea00` |
| Sodium | `u1OEbNKx` | `sodium-fabric-0.6.13+mc1.21.1.jar` | `928a2598178c3a58b0638bab842f467d2e49251a` |

工作区入口日志：`.tools/modrinth-acceptance.log` 和 `.tools/modrinth-corrected.log`。

已证明：真实平台查询、下载校验、必需依赖展开、指定版本更正、管理记录和 Fabric 包内范围预检。

仍未证明：该组合实际图形运行、任意整合包兼容、Forge / Quilt / NeoForge 包内依赖检查，以及复杂依赖的自动回溯求解。预检对于有多个候选的嵌套依赖不构成完整的 SAT 求解器。

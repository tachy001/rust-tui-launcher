# Modrinth 安装验收与依赖修正

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

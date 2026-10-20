# Forge 安装验收

日期：2026-10-19。环境：Windows，tui-mc 0.10.0，Minecraft 1.21.1，Forge 52.1.20。

本次只执行一次真实安装，复用已有原版、资源对象与 Mojang Java 21。没有启动 Minecraft，也没有重新运行单元测试或重新构建程序。

执行入口：

```powershell
.\target\release\tui-mc.exe forge 1.21.1 52.1.20 --game-dir .tools/smoke-game
```

结果：进程退出码 0。官方安装器报告 `Successfully installed client into launcher.`，启动器随后完成依赖与 3888 项资源校验，最终输出 `Forge 实例已就绪：1.21.1-forge-52.1.20`。

已检查的产物：

- `versions/1.21.1-forge-52.1.20/1.21.1-forge-52.1.20.json`：继承 `1.21.1`，主类 `net.minecraftforge.bootstrap.ForgeBootstrap`，49 项加载器库。
- `libraries/net/minecraftforge/forge/1.21.1-52.1.20/forge-1.21.1-52.1.20-client.jar`：官方安装器生成的补丁客户端。
- 安装器下载缓存、安装过程日志以及原版共享资源均保留在 `.tools/smoke-game` 中。

完整入口日志位于工作区 `.tools/forge-acceptance.log`。安装器自己的输出位于 `.tools/smoke-game/tui-mc-installers/forge-1.21.1-52.1.20.log`。

该证据证明这一个版本的安装流程成功，不证明 Forge 图形客户端、任意 Mod、其他 Forge 历史版本、NeoForge 或其他操作系统均已通过。按用户要求，本次没有追加游戏启动验证。

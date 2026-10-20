# NeoForge 安装验收

日期：2026-10-19。环境：Windows，tui-mc 0.11.0，Minecraft 1.21.1，NeoForge 21.1.264。

本次只执行一次真实安装，复用已有原版、资源缓存、共享依赖与 Mojang Java 21。没有启动 Minecraft、重新构建或重跑单元测试。

执行入口：

```powershell
.\target\release\tui-mc.exe neoforge 1.21.1 21.1.264 --game-dir .tools/smoke-game
```

结果：进程退出码 0。官方安装器报告 `Successfully installed client into launcher.`，启动器随后完成 106 项依赖和 3888 项资源校验，最终输出 `NeoForge 实例已就绪：neoforge-21.1.264`。

已检查的产物：

- `versions/neoforge-21.1.264/neoforge-21.1.264.json`：继承 `1.21.1`，主类 `cpw.mods.bootstraplauncher.BootstrapLauncher`，47 项加载器库。
- `libraries/net/neoforged/neoforge/21.1.264/neoforge-21.1.264-client.jar`：5,698,524 字节。
- 同目录 `neoforge-21.1.264-universal.jar`：3,558,897 字节。

完整入口日志位于工作区 `.tools/neoforge-acceptance.log`。安装器输出位于 `.tools/smoke-game/tui-mc-installers/neoforge-21.1.264.log`。

该证据证明这一组游戏与加载器版本的安装成功，不证明游戏图形运行、任意 Mod、其他版本或其他操作系统均已通过。本次按用户要求，没有追加游戏启动验证。

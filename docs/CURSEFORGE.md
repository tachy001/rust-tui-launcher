# CurseForge 整合包

`import` 会根据 ZIP 内的清单识别 Modrinth 或 CurseForge，无需手动选择格式。`pack-info` 可离线查看格式、游戏版本和文件数量。

```console
tui-launcher pack-info MyPack.zip
tui-launcher import MyPack.zip my-instance
```

也可在本地 / 下载页按 I 输入 ZIP 路径。

## API 密钥

CurseForge 的文件查询需要自己的 API 密钥。可以在账户页按 **Shift+K**，以隐藏输入方式保存到操作系统安全存储。该密钥不会写入 `launcher.json` 或账户索引。

也可以由运行环境提供 `CURSEFORGE_API_KEY`；环境变量优先于已保存密钥。用于脚本时可通过标准输入保存：

```console
tui-launcher curseforge key --stdin
tui-launcher curseforge clear-key
```

`key --stdin` 等待输入结束，适合管道或重定向；交互使用建议直接在 TUI 输入。`clear-key` 只移除系统存储中的密钥，不清除进程环境变量。

密钥仅发送给固定的 `api.curseforge.com`，API 请求不跟随重定向；后续 CDN 文件请求不附带密钥。

## 无法自动下载的文件

若作者未提供第三方下载地址，启动器不会猜测 CDN 地址，也不会将残缺实例标记为完成。它会在游戏根目录生成 `tui-mc-manual-downloads.json`，包含项目 / 文件 ID、文件名、SHA-1、大小和需要放置的路径。

从作者页面取得对应文件后，按清单放入 `tui-mc-manual-files/`，然后对同一包再次执行导入。只有 SHA-1 和大小均匹配的文件会被使用。若 API 根本没有返回该文件，清单中只能显示项目 / 文件 ID，仍需解决访问权限或失效文件问题。

## 修复与缓存

成功导入后会保存来源归档缓存，以及实例内的 `tui-mc-pack-receipt.json`。本地页按 X 或执行 `repair <实例>`，除游戏依赖外也会修复整合包文件：

- 下载文件缺失或损坏时重新下载并校验。
- 已禁用的 `.jar.disabled` 会保留禁用状态。
- 内嵌 Mod 可从原包缓存恢复；已有配置的修改保留，只有配置缺失时才恢复原值。
- 手动下载的受限文件会保留已校验的本地缓存，后续修复不重新请求下载地址。

缓存位于游戏根目录 `tui-mc-packs/`。删除缓存后仍可修复能联网下载的文件，但无法恢复只有本地归档或手动文件缓存才有的内容。

## 验证范围

已覆盖 CurseForge 清单转换、主加载器选择、API 返回文件身份与校验字段的本地检查；Modrinth 的完整本地 ZIP 导入和修复也已通过。当前没有 CurseForge API 密钥，因此带实际平台权限的端到端联网导入仍未验证。

接口参考：[CurseForge 官方 API 文档](https://docs.curseforge.com/rest-api/)。

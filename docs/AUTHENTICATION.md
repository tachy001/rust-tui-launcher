# 微软账户配置

启动器实现了设备代码 OAuth → Xbox Live → XSTS → Minecraft 令牌 → 授权资格与档案查询的流程，以及到期刷新。

你需要使用自己注册的应用，当前项目不包含默认客户端 ID，也不会借用 HMCL、PCL 或其他启动器的应用身份。

1. 在 Microsoft Entra 的应用注册中创建支持个人 Microsoft 账户的应用。
2. 为应用启用公共客户端流程，确认它允许设备代码认证，并具备所需 Xbox / Minecraft 服务权限。只有 GUID 并不能保证服务准入。
3. 将 Application (client) ID 填入 TUI 设置页的“微软 OAuth 客户端 ID”，或执行：

   ```console
   tui-launcher account client-id <你的GUID>
   ```

4. 在账户页按 A，或执行 `tui-launcher account login`。
5. 在自己的浏览器打开启动器显示的微软地址，输入一次性代码，登录并授权。

启动器只显示设备登录地址与用户代码，不收集微软密码。登录等待可以用 C（TUI）或 Ctrl+C（CLI）取消。拒绝、过期、Xbox 账户限制与缺少游戏授权会显示对应错误。

## 存储

- `.tui-mc/accounts.json` 仅保存账户名称、UUID、类型、应用 ID、令牌到期时间和 XUID。
- 微软刷新令牌和 Minecraft 访问令牌分别保存在操作系统安全凭证存储中，服务名为 `tui-launcher`。
- Windows 使用 Credential Manager；macOS 使用 Keychain；Linux 使用 Secret Service（需要可用的桌面密钥环及 D-Bus，构建时需要 D-Bus 开发库）。
- 安全存储不可用时直接报错，不退回明文文件。
- 移除微软账户会删除该账户对应的本机凭证；这不撤销微软网站上的应用授权。

## 账户操作

```console
tui-launcher account offline Player
tui-launcher account list
tui-launcher account select <账户ID>
tui-launcher account remove <账户ID>
```

没有选择账户时沿用配置中的离线用户名。已经选择的账户优先于 `username` 设置。离线账户不能代替正版服务器的身份验证。

## 验证状态

协议分支、账户索引中不含令牌、客户端 ID 与档案格式都有本地测试。由于当前没有应用客户端 ID 和测试账户，真实微软 / Xbox / Minecraft 登录与系统安全存储的完整往返尚未验证。

参考：[Microsoft 设备代码流程](https://learn.microsoft.com/en-us/entra/identity-platform/v2-oauth2-device-code)、[keyring 存储后端](https://docs.rs/keyring/3.6.3/keyring/)。

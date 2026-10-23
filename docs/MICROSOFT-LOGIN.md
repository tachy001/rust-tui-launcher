# 微软浏览器登录

账户页按 **A**，或运行 `tui-launcher account login`：启动器请求系统浏览器打开微软授权页，用户在网页选择账户 / 登录并授权，网页回到本机后启动器自动完成 Xbox、Minecraft 资格检查和账户保存。密码不经过启动器。等待最长 10 分钟，任务取消会关闭本次回调监听。浏览器未打开时可复制任务日志中的登录链接。

这是授权码 + PKCE S256 流程。每次登录使用独立安全随机 state / verifier、本机 IPv4 回环监听和随机端口。只接受本次 state、指定回调路径及 Host；无关 / 伪造请求不会完成登录。回调页只说明收到微软授权，不在 Xbox / Minecraft 验证之前声称整个登录成功。刷新令牌仍按账户原有的客户端 ID 刷新，存于系统安全凭证存储。

## 发布者的一次性配置

应用 ID 是**启动器发布者注册的应用标识**，不是玩家的账户 ID，也不是密码或客户端密钥。不能省略微软的应用注册，也不应借用 HMCL / PCL 的应用 ID。

1. 在 Microsoft Entra 注册本启动器应用，支持个人 Microsoft 账户。
2. 在认证配置中添加“移动和桌面应用”回调地址 `http://localhost/auth-response`，使用公共客户端，不给可执行文件嵌入 client secret。运行时回调使用动态端口；注册地址的主机名、路径应与此一致。
3. GitHub 仓库 **Settings → Secrets and variables → Actions → Variables** 添加 `MICROSOFT_CLIENT_ID`，值为该应用的 GUID。Release 工作流将其作为 `RUST_TUI_LAUNCHER_MICROSOFT_CLIENT_ID` 编译入三个平台的程序。该 ID 是公开标识，可用仓库变量保存。
4. 本地构建时设置同名环境变量 `RUST_TUI_LAUNCHER_MICROSOFT_CLIENT_ID` 后编译。修改变量后需要重新构建已有程序。
5. 用真实正版账户验证微软授权、Xbox / Minecraft API 和刷新 / 系统凭证存储；应用注册成功不能代替完整验收。

普通用户保持“微软应用 ID（高级覆盖）”为空即可使用内置 ID。高级覆盖仅供开发者、自编译或自有应用迁移使用；覆盖项优先于内置值。两者都为空的构建会明确提示发布者配置缺失，不会诱导玩家自行申请 ID，也不会伪装成可以登录。

## 备用设备码方式

无法使用本地回调、远程终端或无浏览器环境下运行 `tui-launcher account login-device`。启动器显示微软网页地址和设备码，尝试打开系统浏览器，并等待网页授权。此方式需要同一应用启用公共客户端设备代码流程。浏览器登录失败时不会自动切换流程，避免开启重复授权；用户可取消后明确选择设备码。

参考：[Microsoft 授权码流程](https://learn.microsoft.com/en-us/entra/identity-platform/v2-oauth2-auth-code-flow)、[桌面应用配置](https://learn.microsoft.com/en-us/entra/identity-platform/scenario-desktop-app-configuration)、[回调地址与 localhost 端口规则](https://learn.microsoft.com/en-us/entra/identity-platform/reply-url)。本项目同时对照本地 HMCL `OAuth.authenticateAuthorizationCode` / `OAuthServer` 的流程职责，独立实现 Rust 服务。

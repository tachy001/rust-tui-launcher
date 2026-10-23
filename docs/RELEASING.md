# GitHub 上传与 Release

## 仓库边界

在包含 `Cargo.toml`、`src/` 和本说明的项目根目录初始化仓库，
不要在同时包含 HMCL / PCL 参考源码的上层目录初始化。

提交源码、Cargo.lock、脚本、文档、许可证和 `.github/`。
`.gitignore` 已排除 `target/`、`.tools/`、`.tui-mc/`、
`.minecraft/`、`launcher.json`、`dist/`、日志和 `.env`。
这些本地文件仍保留在磁盘。自定义游戏目录也应放在仓库外，
或另加忽略规则。`Cargo.lock` 必须提交，发布构建使用 `--locked`。

## 首次上传

先在 GitHub 创建空仓库，将下面的 OWNER 和 REPO 换成实际值：

```sh
git init -b main
git add .
git status --short
git diff --cached --stat
git commit -m "Initial release preparation"
git remote add origin https://github.com/OWNER/REPO.git
git push -u origin main
```

项目名称为 Rust TUI Launcher，GitHub 仓库名为 `rust-tui-launcher`。
提交前查看暂存清单，确认没有游戏文件、账号索引、令牌或 API 密钥。
此文档不会自动创建仓库或上传内容。

## 打包

Windows、macOS、Linux 的源码目录都可运行以下命令；
Python 需 3.11 或更新版本：

```sh
cargo build --locked --release
python scripts/package_release.py --platform windows-x64
```

`--platform` 改为本机实际构建平台：`windows-x64`、`linux-x64`、
`linux-arm64`、`macos-x64`、`macos-arm64`。
该参数只标注包的平台，不会交叉编译，不能用 Windows 程序冒充其他平台。

产物在 `dist/`，名称自动读取 Cargo 项目名和版本：
`项目名-v版本-平台.exe`（Windows）或 `.tar.gz`（Unix）。
每个包都有同名 `.sha256` 校验文件，仅用于本地校验，
不作为 Release 附件上传。
Windows 直接提供独立 EXE；Unix 包内仅包含可执行文件，解压后运行 `./tui-launcher`；
不会打包配置、账号、游戏资源或工具链。压缩包使用独立顶层目录。

## 发布草稿

1. 更新 `Cargo.toml` 版本和 `Cargo.lock`，补充 `CHANGELOG.md`。
2. 按 [微软登录配置](MICROSOFT-LOGIN.md) 设置仓库变量 `MICROSOFT_CLIENT_ID`（发布者的一次性应用配置）；提交并推送。普通分支推送不触发工作流；PR 在三端检查格式、Clippy 和本地测试，手动触发还会生成三个平台的打包附件。
3. 创建与 Cargo 版本完全一致的新标签；将下面的 `vX.Y.Z` 换成实际版本：

   ```sh
   git tag -a vX.Y.Z -m "vX.Y.Z"
   git push origin vX.Y.Z
   ```

4. 标签构建通过后，工作流自动创建并发布名为 **Rust TUI Launcher v版本** 的 Release，只上传三个包：
   Windows x64 `.exe`、Linux x64 `.tar.gz`、macOS Universal `.tar.gz`。
   macOS 包包含通过 `lipo` 合并并检查的 Intel / Apple Silicon 双架构程序。
   任一平台失败，不会进入发布阶段。标签与 Cargo 版本不匹配会在编译前失败。
5. 在 GitHub Releases 检查附件、说明和实测范围。

PR 只做检查，不重复编译 Release 或上传包；手动运行会生成 Actions 构建附件，不创建 Release。
失败重跑可继续更新尚未发布的草稿：先上传这三个包，成功后清除草稿中的其他附件并发布。工作流不会覆盖已经公开发布的同标签 Release；新版本使用新的 Cargo 版本和标签。

GitHub 自动提供的 Source code 下载入口由平台生成，不属于工作流上传的附件。
Linux ARM64 仍可自行源码构建，暂不提供单独的 Release 包。

原生 ARM64 的构建通过不等于每个游戏版本和 Mod 都支持 ARM64。
完整平台限制见 [PLATFORMS.md](PLATFORMS.md)。

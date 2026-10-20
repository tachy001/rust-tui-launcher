use crate::{
    accounts::{AccountKind, Accounts},
    backend::{self, Backend, Message},
    config::Config,
    java,
    loader::Loader,
    mods,
    task::TaskControl,
    ui, version,
};
use anyhow::{bail, Context, Result};
use std::{path::Path, sync::mpsc, thread};

const HELP: &str = "Tui-Launcher — Minecraft Java 终端启动器

不带命令进入交互界面。
  list                         列出本地版本
  remote [搜索词]              官方版本目录（--snapshots 包含快照）
  install <版本>               安装原版
  repair <实例>                校验修复原版或继承实例
  fabric <版本> [加载器版本]   安装 Fabric（默认稳定版）
  quilt <版本> [加载器版本]    安装 Quilt
  forge <版本> [加载器版本]    使用官方安装器安装 Forge
  neoforge <版本> [加载器版本] 使用官方安装器安装 NeoForge
  loader-versions <类型> <游戏版本>  列出 Fabric / Quilt / Forge / NeoForge 版本
  launch <实例>                启动，Ctrl+C 终止游戏
  show <实例>                  查看解析后的版本信息
  diagnose <实例>              分析最近日志与崩溃报告，不启动游戏
  world <实例> list            列出实例存档
  world <实例> backup <目录名> <ZIP路径>  备份世界，不覆盖已有 ZIP
  world <实例> import <ZIP路径> <新目录名>  导入世界，不覆盖现有世界
  java                         查找本机 Java
  java-install <主版本或实例>  安装 Mojang Java（如 8、17、21、25）
  import <mrpack或ZIP> [实例ID] 导入 Modrinth / CurseForge 整合包
  pack-info <mrpack或ZIP>       查看整合包格式与版本
  export <实例> <mrpack路径>   导出 Mod / 配置，不包含存档与账户
  instance name <ID> <名称>   设置实例显示名称
  instance clone <ID> <新ID>  复制实例数据（含存档）
  instance remove <ID>         移至回收站，保留全部实例数据
  instance trash               列出回收站
  instance restore <回收站ID>  恢复实例
  doctor / --doctor            诊断配置、Java 和本地版本
  mods <实例> list             列出 Mod
  mods <实例> add <JAR路径>    导入 Mod（不覆盖）
  mods <实例> toggle <文件名>  启用 / 禁用 Mod
  mods <实例> remove <文件名>  移出 Mod 并保留备份，修复不再恢复该文件
  mods <实例> removed          列出可恢复的 Mod 备份
  mods <实例> restore <编号>   恢复 Mod 与对应管理记录，不覆盖新文件
  mods <实例> search <关键词>  在线搜索兼容 Mod
  mods <实例> versions <项目>  列出兼容版本 ID
  mods <实例> download <项目> [版本ID]  安装项目与必需依赖
  mods <实例> updates          检查在线管理项目的更新
  mods <实例> check            检查 Fabric 包内依赖范围，不启动游戏
  account list                 列出账户
  account offline <用户名>     添加并选择离线账户
  account select <账户ID>      切换账户
  account remove <账户ID>      移除账户及安全凭证
  account login                微软设备代码登录
  account client-id <GUID>      配置自己的 OAuth 应用 ID
  curseforge key --stdin        从标准输入保存自己的 API 密钥
  curseforge clear-key          移除已保存 API 密钥

全局选项（本次有效，不覆盖配置）：
  --game-dir <路径>  --java <auto或路径>  --username <用户名>
  --memory <MB>  --threads <1-16>  --shared  --isolated
  --source <official|bmclapi|HTTPS根地址>  --no-optional
  --fullscreen / --windowed  --server <主机[:端口]>  --no-server
";
struct Options {
    config: Config,
    args: Vec<String>,
    snapshots: bool,
}
fn parse(mut config: Config, raw: Vec<String>) -> Result<Options> {
    let mut args = vec![];
    let mut snapshots = false;
    let mut fullscreen_override = None;
    let mut server_override = None;
    let mut iter = raw.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--game-dir" => config.game_dir = iter.next().context("--game-dir 缺少路径")?.into(),
            "--java" => config.java = iter.next().context("--java 缺少值")?,
            "--username" => config.username = iter.next().context("--username 缺少用户名")?,
            "--memory" => {
                config.memory_mb = iter
                    .next()
                    .context("--memory 缺少值")?
                    .parse()
                    .context("内存应为整数")?
            }
            "--threads" => {
                config.download_threads = iter
                    .next()
                    .context("--threads 缺少值")?
                    .parse()
                    .context("线程数应为整数")?
            }
            "--source" => config.download_source = iter.next().context("--source 缺少值")?,
            "--no-optional" => config.optional_pack_files = false,
            "--shared" => config.isolated = false,
            "--isolated" => config.isolated = true,
            "--snapshots" => snapshots = true,
            "--fullscreen" => {
                config.fullscreen = true;
                fullscreen_override = Some(true);
            }
            "--windowed" => {
                config.fullscreen = false;
                fullscreen_override = Some(false);
            }
            "--server" => {
                config.server = iter.next().context("--server 缺少地址")?;
                server_override = Some(config.server.clone());
            }
            "--no-server" => {
                config.server.clear();
                server_override = Some(String::new());
            }
            _ => args.push(arg),
        }
    }
    config.validate()?;
    if args.first().is_some_and(|command| command == "launch") {
        if let Some(id) = args.get(1) {
            let instance = config.instances.entry(id.clone()).or_default();
            if let Some(fullscreen) = fullscreen_override {
                instance.fullscreen = Some(fullscreen);
            }
            if let Some(server) = server_override {
                instance.server = Some(server);
            }
        }
    }
    Ok(Options {
        config,
        args,
        snapshots,
    })
}
pub fn run(config: Config, raw: Vec<String>) -> Result<()> {
    let Options {
        config,
        args,
        snapshots,
    } = parse(config, raw)?;
    if args.is_empty() {
        return ui::run(config);
    }
    if matches!(args[0].as_str(), "help" | "--help" | "-h") {
        println!("{HELP}");
        return Ok(());
    }
    match args[0].as_str() {
        "world" if args.get(2).is_some_and(|action| action == "list") => {
            exact(&args, 3)?;
            for name in crate::worlds::list(&config, &args[1])? {
                println!("{name}");
            }
            return Ok(());
        }
        "curseforge" => {
            match args.get(1).map(String::as_str) {
                Some("key") if args.get(2).is_some_and(|flag| flag == "--stdin") => {
                    exact(&args, 3)?;
                    use std::io::Read;
                    let mut key = zeroize::Zeroizing::new(String::new());
                    std::io::stdin().take(8192).read_to_string(&mut key)?;
                    crate::curseforge::save_key(&key)?;
                    println!("CurseForge 密钥已保存到系统安全存储");
                }
                Some("clear-key") => {
                    exact(&args, 2)?;
                    crate::curseforge::clear_key()?;
                }
                _ => bail!("用法：curseforge key --stdin | curseforge clear-key"),
            };
            return Ok(());
        }
        "pack-info" => {
            exact(&args, 2)?;
            let pack = crate::pack::inspect(Path::new(&args[1]))?;
            println!(
                "{} · {} · Minecraft {} · {} 个文件",
                pack.name, pack.format, pack.minecraft, pack.files
            );
            return Ok(());
        }
        "list" => {
            exact(&args, 1)?;
            for id in backend::installed(&config) {
                println!("{id}");
            }
            return Ok(());
        }
        "diagnose" => {
            exact(&args, 2)?;
            println!(
                "{}",
                crate::diagnostics::analyze(&config, &args[1], None)?.summary()
            );
            return Ok(());
        }
        "loader-versions" => {
            exact(&args, 3)?;
            let (tx, _rx) = mpsc::channel();
            let backend = Backend::new(tx)?;
            let kind = crate::loader_catalog::InstallerKind::parse(&args[1])?;
            for entry in backend.loader_versions(kind, &args[2])? {
                println!(
                    "{}{}",
                    entry.version,
                    if entry.stable { "" } else { " [预览]" }
                );
            }
            return Ok(());
        }
        "instance" => {
            let action = args
                .get(1)
                .context("用法：instance name / clone / remove / trash / restore")?;
            match action.as_str() {
                "name" => {
                    exact(&args, 4)?;
                    crate::instances::set_name(&config, &args[2], &args[3])?;
                }
                "remove" => {
                    exact(&args, 3)?;
                    let entry = crate::instances::trash(&config, &args[2])?;
                    println!("实例已保留在回收站，恢复 ID：{}", entry.token);
                }
                "restore" => {
                    exact(&args, 3)?;
                    println!("已恢复：{}", crate::instances::restore(&config, &args[2])?);
                }
                "trash" => {
                    exact(&args, 2)?;
                    for entry in crate::instances::trash_list(&config)? {
                        println!("{} · {} · {}", entry.token, entry.id, entry.label);
                    }
                }
                "clone" => {}
                _ => bail!("未知实例管理命令"),
            }
            if action != "clone" {
                return Ok(());
            }
        }
        "java" | "doctor" | "--doctor" => {
            exact(&args, 1)?;
            println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
            println!("游戏目录: {}", config.game_dir.display());
            println!("Java 设置: {}", config.java);
            if args[0] != "java" {
                println!("本地版本: {}", backend::installed(&config).join(", "));
            }
            let found = java::discover(&config);
            for runtime in &found {
                println!(
                    "Java {} · {} · {}",
                    runtime.major,
                    runtime.path.display(),
                    runtime.description
                );
            }
            if found.is_empty() {
                bail!("未发现 Java");
            }
            return Ok(());
        }
        "show" => {
            exact(&args, 2)?;
            let resolved = version::resolve(&config.root()?, &args[1])?;
            println!(
                "版本链：{}\n客户端 JAR：{}",
                resolved.chain.join(" → "),
                resolved.jar_id
            );
            println!("{}", serde_json::to_string_pretty(&resolved.meta)?);
            return Ok(());
        }
        "mods"
            if !args.get(2).is_some_and(|action| {
                matches!(
                    action.as_str(),
                    "search" | "versions" | "download" | "updates"
                )
            }) =>
        {
            if args.len() < 3 {
                bail!("用法：mods <实例> list | add <路径> | toggle <文件名>");
            }
            match args[2].as_str() {
                "list" => {
                    exact(&args, 3)?;
                    for entry in mods::list(&config, &args[1])? {
                        println!(
                            "{} {:>8} {}",
                            if entry.enabled { "启用" } else { "禁用" },
                            entry.size,
                            entry.filename
                        );
                    }
                }
                "add" => {
                    exact(&args, 4)?;
                    mods::import(&config, &args[1], Path::new(&args[3]))?;
                }
                "toggle" => {
                    exact(&args, 4)?;
                    mods::toggle(&config, &args[1], &args[3])?;
                }
                "check" => {
                    exact(&args, 3)?;
                    let target = crate::modrinth::target(&config, &args[1])?;
                    if target.loader != "fabric" {
                        bail!("当前包内依赖检查适用于 Fabric 实例");
                    }
                    let resolved = version::resolve(&config.root()?, &args[1])?;
                    let builtins =
                        crate::fabric_check::builtins(&resolved.meta, &target.minecraft, None);
                    crate::fabric_check::audit(
                        &crate::fabric_check::active_paths(&config, &args[1])?,
                        &builtins,
                    )?;
                    println!("Fabric 包内依赖检查通过；Java 要求在启动前按实际运行时检查");
                }
                "remove" => {
                    exact(&args, 4)?;
                    println!(
                        "Mod 已移出，备份：{}",
                        mods::remove(&config, &args[1], &args[3])?.display()
                    );
                }
                "removed" => {
                    exact(&args, 3)?;
                    for entry in mods::removed(&config, &args[1])? {
                        println!("{} {}", entry.token, entry.filename);
                    }
                }
                "restore" => {
                    exact(&args, 4)?;
                    println!("已恢复 {}", mods::restore(&config, &args[1], &args[3])?);
                }
                _ => bail!("未知 Mod 命令"),
            };
            return Ok(());
        }
        "account" if args.get(1).is_some_and(|action| action != "login") => {
            let mut accounts = Accounts::load()?;
            match args[1].as_str() {
                "list" => {
                    exact(&args, 2)?;
                    for entry in &accounts.entries {
                        println!(
                            "{} {} · {} · {}",
                            if accounts.active.as_ref() == Some(&entry.id) {
                                "*"
                            } else {
                                " "
                            },
                            entry.id,
                            entry.name,
                            if entry.kind == AccountKind::Microsoft {
                                "微软"
                            } else {
                                "离线"
                            }
                        );
                    }
                }
                "offline" => {
                    exact(&args, 3)?;
                    accounts.add_offline(&args[2])?;
                }
                "select" => {
                    exact(&args, 3)?;
                    accounts.select(&args[2])?;
                }
                "remove" => {
                    exact(&args, 3)?;
                    accounts.remove(&args[2])?;
                }
                "client-id" => {
                    exact(&args, 3)?;
                    let mut saved = Config::load()?;
                    saved.microsoft_client_id = args[2].clone();
                    saved.save()?;
                    println!("OAuth 客户端 ID 已保存");
                }
                _ => bail!("未知账户命令，请使用 --help"),
            }
            return Ok(());
        }
        _ => {}
    }
    let control = TaskControl::default();
    let signal = control.clone();
    ctrlc::set_handler(move || signal.cancel()).context("无法安装 Ctrl+C 处理器")?;
    let (tx, rx) = mpsc::channel();
    let logger = thread::spawn(move || {
        let mut previous = String::new();
        let mut last_transfer = std::time::Instant::now() - std::time::Duration::from_secs(2);
        for message in rx {
            match message {
                Message::Transfer(transfer) => {
                    if transfer.speed > 0.0
                        && (last_transfer.elapsed() >= std::time::Duration::from_secs(2)
                            || transfer.total == Some(transfer.done))
                    {
                        eprintln!("{}", transfer.display(false));
                        last_transfer = std::time::Instant::now();
                    }
                }
                Message::Log(s) => eprintln!("{s}"),
                Message::Progress(label, n, total) => {
                    let bucket = n.saturating_mul(10).checked_div(total).unwrap_or(0);
                    let key = format!("{label}:{bucket}");
                    if key != previous {
                        eprintln!("{label}: {n}/{total}");
                        previous = key;
                    }
                }
                _ => {}
            }
        }
    });
    let result = (|| -> Result<()> {
        let mut backend = Backend::new(tx.clone())?;
        backend.control = control;
        backend.source = config.download_source.clone();
        match args[0].as_str() {
            "world" => {
                exact(&args, 5)?;
                match args[2].as_str() {
                    "backup" => {
                        backend.backup_world(&config, &args[1], &args[3], Path::new(&args[4]))?;
                        println!("存档已备份：{}", args[4]);
                    }
                    "import" => {
                        backend.import_world(&config, &args[1], Path::new(&args[3]), &args[4])?;
                        println!("存档已导入：{}", args[4]);
                    }
                    _ => bail!("用法：world <实例> list | backup <目录名> <ZIP路径> | import <ZIP路径> <新目录名>"),
                }
            }
            "mods" => {
                let id = args.get(1).context("未指定实例")?;
                match args.get(2).map(String::as_str) {
                    Some("search") => {
                        exact(&args, 4)?;
                        let result = backend.search_mods(&config, id, &args[3], 0)?;
                        for hit in result.hits {
                            println!("{} · {} · {}", hit.project_id, hit.title, hit.description);
                        }
                    }
                    Some("versions") => {
                        exact(&args, 4)?;
                        for version in backend.mod_versions(&config, id, &args[3])? {
                            println!(
                                "{} · {} · {} · {}",
                                version.id,
                                version.version_number,
                                version.version_type,
                                version.date_published
                            );
                        }
                    }
                    Some("download") => {
                        if !(4..=5).contains(&args.len()) {
                            bail!("用法：mods <实例> download <项目> [版本ID]");
                        }
                        println!(
                            "{}",
                            backend.install_mod(
                                &config,
                                id,
                                &args[3],
                                args.get(4).map(String::as_str)
                            )?
                        );
                    }
                    Some("updates") => {
                        exact(&args, 3)?;
                        for hit in backend.mod_updates(&config, id)? {
                            println!("{} · {} · {}", hit.project_id, hit.title, hit.description);
                        }
                    }
                    _ => bail!("未知在线 Mod 命令"),
                };
            }
            "export" => {
                exact(&args, 3)?;
                backend.export_pack(&config, &args[1], Path::new(&args[2]))?;
            }
            "instance" => {
                exact(&args, 4)?;
                crate::instances::clone_instance(&config, &args[2], &args[3], &backend.control)?;
                println!("新实例：{}", args[3]);
            }
            "import" => {
                if !(2..=3).contains(&args.len()) {
                    bail!("用法：import <mrpack路径> [实例ID]");
                }
                println!(
                    "整合包实例：{}",
                    backend.import_pack(
                        &config,
                        Path::new(&args[1]),
                        args.get(2).map(String::as_str)
                    )?
                );
            }
            "account" => {
                exact(&args, 2)?;
                if args[1] != "login" {
                    bail!("使用 account login 登录");
                }
                println!("{}", backend.login_microsoft(&config)?);
            }
            "remote" => {
                if args.len() > 2 {
                    bail!("用法：remote [搜索词]");
                }
                let query = args.get(1).map(|s| s.to_lowercase()).unwrap_or_default();
                for version in backend.manifest()?.versions {
                    if (snapshots || version.kind == "release")
                        && version.id.to_lowercase().contains(&query)
                    {
                        println!("{:<26} {}", version.id, version.kind);
                    }
                }
            }
            "install" => {
                exact(&args, 2)?;
                let remote = backend.remote_version(&args[1])?;
                backend.install(&config, &remote)?;
            }
            "repair" => {
                exact(&args, 2)?;
                backend.repair(&config, &args[1])?;
            }
            "launch" => {
                exact(&args, 2)?;
                backend.launch(&config, &args[1])?;
            }
            "java-install" => {
                exact(&args, 2)?;
                let runtime = if let Ok(major) = args[1].parse::<u64>() {
                    backend.install_java(&config, java::component_for(major)?, major)?
                } else {
                    backend.install_java_for_version(&config, &args[1])?
                };
                println!("Java {} 已安装：{}", runtime.major, runtime.path.display());
            }
            "fabric" | "quilt" => {
                if !(2..=3).contains(&args.len()) {
                    bail!("用法：{} <游戏版本> [加载器版本]", args[0]);
                }
                let loader = if args[0] == "fabric" {
                    Loader::Fabric
                } else {
                    Loader::Quilt
                };
                let id = backend.install_loader(
                    &config,
                    loader,
                    &args[1],
                    args.get(2).map(String::as_str),
                )?;
                println!("实例已安装：{id}");
            }
            "forge" | "neoforge" => {
                if !(2..=3).contains(&args.len()) {
                    bail!("用法：{} <游戏版本> [加载器版本]", args[0]);
                }
                let kind = if args[0] == "forge" {
                    crate::forge::ForgeKind::Forge
                } else {
                    crate::forge::ForgeKind::NeoForge
                };
                let id = backend.install_forge(
                    &config,
                    kind,
                    &args[1],
                    args.get(2).map(String::as_str),
                )?;
                println!("实例已安装：{id}");
            }
            other => bail!("未知命令：{other}，使用 --help 查看帮助"),
        }
        Ok(())
    })();
    drop(tx);
    let _ = logger.join();
    result
}
fn exact(args: &[String], expected: usize) -> Result<()> {
    if args.len() != expected {
        bail!("参数数量不正确，使用 --help 查看帮助");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn launch_flags_override_saved_instance_options_without_saving() {
        let mut config = Config::default();
        config.instances.insert(
            "test".into(),
            crate::config::InstanceSettings {
                fullscreen: Some(true),
                server: Some("old.example.org".into()),
                ..Default::default()
            },
        );
        let options = parse(
            config,
            ["launch", "test", "--windowed", "--no-server"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        )
        .unwrap();
        let effective = options.config.effective("test");
        assert!(!effective.fullscreen);
        assert!(effective.server.is_empty());
        let options = parse(
            Config::default(),
            ["launch", "test", "--fullscreen", "--server", "[::1]:25566"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        )
        .unwrap();
        assert!(options.config.effective("test").fullscreen);
        assert_eq!(options.config.effective("test").server, "[::1]:25566");
    }
    #[test]
    fn options_do_not_consume_command_arguments() {
        let options = parse(
            Config::default(),
            ["--game-dir", "a b", "fabric", "1.21.1", "--threads", "4"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        )
        .unwrap();
        assert_eq!(options.args, ["fabric", "1.21.1"]);
        assert_eq!(options.config.download_threads, 4);
        assert_eq!(options.config.game_dir, Path::new("a b"));
    }
    #[test]
    fn invalid_options_fail() {
        assert!(parse(Config::default(), vec!["--memory".into()]).is_err());
        assert!(parse(Config::default(), vec!["--threads".into(), "0".into()]).is_err());
    }
}

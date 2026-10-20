use crate::{
    accounts::{AccountKind, Accounts},
    backend::{self, Backend, Message},
    config::Config,
    java,
    loader::Loader,
    metadata::RemoteVersion,
    mods::{self, ModEntry},
    task::TaskControl,
    version,
};
use anyhow::{bail, Context, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Wrap,
    },
    Frame, Terminal,
};
use std::{
    collections::{HashMap, VecDeque},
    io,
    path::Path,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::Duration,
};

const ACCENT: Color = Color::Rgb(115, 224, 173);
const MUTED: Color = Color::Rgb(139, 155, 178);
const BG: Color = Color::Rgb(16, 22, 32);
const TEXT: Color = Color::Rgb(220, 231, 242);
const SETTINGS: [&str; 15] = [
    "离线用户名",
    "Java 路径（auto 自动匹配）",
    "最大内存 / MB",
    "游戏目录",
    "版本隔离（true / false）",
    "JVM 附加参数",
    "游戏附加参数",
    "并发下载数",
    "窗口宽度",
    "窗口高度",
    "微软 OAuth 客户端 ID",
    "下载源（official / bmclapi / HTTPS）",
    "整合包可选文件（true / false）",
    "全屏启动（true / false）",
    "直连服务器（留空进入菜单）",
];
#[derive(Clone, Copy, PartialEq)]
enum WorkKind {
    Download,
    Game,
    Other,
}
#[derive(Clone, Copy)]
enum EditKind {
    Search,
    Setting,
    ImportMod,
    OfflineAccount,
    ImportPack,
    NameInstance,
    CloneInstance,
    ExportPack,
    CurseKey,
    ModQuery,
    BackupWorld,
    ImportWorldZip,
    ImportWorldName,
}
struct Editor {
    kind: EditKind,
    text: String,
}
struct LoaderPicker {
    kind: crate::loader_catalog::InstallerKind,
    game: String,
    versions: Vec<crate::loader_catalog::LoaderVersion>,
    selected: usize,
}
struct WorldPicker {
    instance: String,
    worlds: Vec<String>,
    selected: usize,
}
impl Drop for Editor {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.text);
    }
}
struct App {
    config: Config,
    tab: usize,
    selection: usize,
    local: Vec<String>,
    remote: Vec<RemoteVersion>,
    logs: VecDeque<String>,
    task: Option<WorkKind>,
    control: TaskControl,
    progress: (String, usize, usize),
    paused: bool,
    snapshots: bool,
    query: String,
    tx: Sender<Message>,
    rx: Receiver<Message>,
    editor: Option<Editor>,
    setting: usize,
    scope: Option<String>,
    mod_instance: Option<String>,
    mod_list: Vec<ModEntry>,
    confirm_stop: bool,
    log_scroll: usize,
    log_horizontal: u16,
    accounts: Accounts,
    help: bool,
    help_scroll: u16,
    pause_allowed: bool,
    names: HashMap<String, String>,
    trash: Vec<crate::instances::TrashEntry>,
    editor_instance: Option<String>,
    confirm_remove: Option<String>,
    mod_search: Option<crate::modrinth::SearchResult>,
    mod_versions: Option<(String, Vec<crate::modrinth::ModVersion>)>,
    mod_query: String,
    mod_updates_only: bool,
    loader_picker: Option<LoaderPicker>,
    transfer: Option<(crate::download_progress::Snapshot, std::time::Instant)>,
    world_picker: Option<WorldPicker>,
    pending_world_zip: Option<std::path::PathBuf>,
}
impl App {
    fn new(config: Config, tx: Sender<Message>, rx: Receiver<Message>) -> Self {
        let local = backend::installed(&config);
        let selection = config
            .last_version
            .as_ref()
            .and_then(|id| local.iter().position(|v| v == id))
            .unwrap_or(0);
        let mod_instance = config.last_version.clone();
        let names = crate::instances::names(&config);
        Self {
            config,
            tab: 0,
            selection,
            local,
            remote: vec![],
            logs: VecDeque::new(),
            task: None,
            control: TaskControl::default(),
            progress: ("空闲".into(), 0, 0),
            paused: false,
            snapshots: false,
            query: String::new(),
            tx,
            rx,
            editor: None,
            setting: 0,
            scope: None,
            mod_instance,
            mod_list: vec![],
            confirm_stop: false,
            log_scroll: 0,
            log_horizontal: 0,
            accounts: Accounts::load().unwrap_or_default(),
            help: false,
            help_scroll: 0,
            pause_allowed: true,
            names,
            trash: vec![],
            editor_instance: None,
            confirm_remove: None,
            mod_search: None,
            mod_versions: None,
            mod_query: String::new(),
            mod_updates_only: false,
            loader_picker: None,
            transfer: None,
            world_picker: None,
            pending_world_zip: None,
        }
    }
    fn log(&mut self, text: impl Into<String>) {
        for line in text.into().lines() {
            self.logs.push_back(line.to_owned());
            if self.logs.len() > 500 {
                self.logs.pop_front();
            }
        }
    }
    fn matches(&self, id: &str) -> bool {
        id.to_lowercase().contains(&self.query.to_lowercase())
    }
    fn local_visible(&self) -> Vec<&String> {
        self.local
            .iter()
            .filter(|id| {
                self.matches(id) || self.names.get(*id).is_some_and(|name| self.matches(name))
            })
            .collect()
    }
    fn remote_visible(&self) -> Vec<&RemoteVersion> {
        self.remote
            .iter()
            .filter(|v| (self.snapshots || v.kind == "release") && self.matches(&v.id))
            .collect()
    }
    fn selected(&self) -> Option<String> {
        match self.tab {
            0 => self
                .local_visible()
                .get(self.selection)
                .map(|id| (*id).clone()),
            1 => self
                .remote_visible()
                .get(self.selection)
                .map(|v| v.id.clone()),
            _ => None,
        }
    }
    fn count(&self) -> usize {
        match self.tab {
            0 => self.local_visible().len(),
            1 => self.remote_visible().len(),
            2 => SETTINGS.len(),
            3 => {
                if let Some((_, versions)) = &self.mod_versions {
                    versions.len()
                } else if let Some(result) = &self.mod_search {
                    result.hits.len()
                } else {
                    self.mod_list.len()
                }
            }
            4 => self.accounts.entries.len(),
            5 => self.trash.len(),
            _ => 0,
        }
    }
    fn work(
        &mut self,
        kind: WorkKind,
        task: impl FnOnce(Backend) -> Result<String> + Send + 'static,
    ) {
        if self.task.is_some() {
            self.log("已有任务正在运行");
            return;
        }
        self.task = Some(kind);
        self.pause_allowed = kind == WorkKind::Download;
        self.control = TaskControl::default();
        self.paused = false;
        self.progress = ("准备中".into(), 0, 0);
        self.transfer = None;
        let tx = self.tx.clone();
        let control = self.control.clone();
        let source = self.config.download_source.clone();
        thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                Backend::new(tx.clone()).and_then(|mut b| {
                    b.control = control;
                    b.source = source;
                    task(b)
                })
            }))
            .unwrap_or_else(|_| Err(anyhow::anyhow!("后台任务异常终止")))
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Message::Done(result));
        });
    }
    fn refresh(&mut self) {
        self.local = backend::installed(&self.config);
        self.names = crate::instances::names(&self.config);
        if self.tab == 1 {
            self.work(WorkKind::Download, |b| {
                b.catalog()?;
                Ok("版本目录已更新".into())
            });
        } else if self.tab == 3 {
            self.refresh_mods();
        } else if self.tab == 5 {
            self.refresh_trash();
        } else {
            self.log("本地版本已刷新");
        }
    }
    fn refresh_trash(&mut self) {
        match crate::instances::trash_list(&self.config) {
            Ok(entries) => self.trash = entries,
            Err(e) => self.log(e.to_string()),
        }
    }
    fn refresh_mods(&mut self) {
        self.mod_list.clear();
        if let Some(id) = &self.mod_instance {
            match mods::list(&self.config, id) {
                Ok(list) => self.mod_list = list,
                Err(e) => self.log(e.to_string()),
            }
        }
    }
    fn action(&mut self) {
        let config = self.config.clone();
        match self.tab {
            0 => {
                if let Some(id) = self.selected() {
                    if self.task.is_some() {
                        self.log("请等待当前任务完成");
                        return;
                    }
                    self.config.last_version = Some(id.clone());
                    if let Err(e) = self.config.save() {
                        self.log(e.to_string());
                        return;
                    }
                    self.work(WorkKind::Game, move |b| {
                        b.launch(&config, &id)?;
                        Ok("游戏已退出".into())
                    });
                }
            }
            1 => {
                if let Some(remote) = self
                    .remote_visible()
                    .get(self.selection)
                    .map(|v| (*v).clone())
                {
                    self.work(WorkKind::Download, move |b| {
                        b.install(&config, &remote)?;
                        Ok(format!("{} 安装完成", remote.id))
                    });
                }
            }
            2 => {
                if self.task.is_none() {
                    self.editor = Some(Editor {
                        kind: EditKind::Setting,
                        text: self.setting_value(self.setting),
                    });
                } else {
                    self.log("请等待任务完成再修改设置");
                }
            }
            3 if self.task.is_none() => {
                if let Some((project, versions)) = &self.mod_versions {
                    if let Some(version) = versions.get(self.selection) {
                        self.install_online_mod(project.clone(), Some(version.id.clone()));
                    }
                } else if let Some(result) = &self.mod_search {
                    if let Some(hit) = result.hits.get(self.selection) {
                        self.install_online_mod(hit.project_id.clone(), None);
                    }
                } else if let (Some(id), Some(entry)) =
                    (&self.mod_instance, self.mod_list.get(self.selection))
                {
                    let result = mods::toggle(&self.config, id, &entry.filename);
                    self.report(result.map(|_| "Mod 状态已更新，下次启动生效".into()));
                    self.refresh_mods();
                }
            }
            4 if self.task.is_none() => {
                if let Some(account) = self.accounts.entries.get(self.selection) {
                    let id = account.id.clone();
                    let result = self.accounts.select(&id);
                    self.report(result.map(|_| "账户已切换".into()));
                }
            }
            5 if self.task.is_none() => {
                if let Some(entry) = self.trash.get(self.selection) {
                    let token = entry.token.clone();
                    let result = crate::instances::restore(&self.config, &token);
                    self.report(result.map(|id| format!("已恢复实例：{id}")));
                    self.local = backend::installed(&self.config);
                    self.names = crate::instances::names(&self.config);
                    self.refresh_trash();
                }
            }
            _ => {}
        }
    }
    fn search_online_mods(&mut self, query: String, offset: u64) {
        if let Some(id) = self.mod_instance.clone() {
            let config = self.config.clone();
            self.work(WorkKind::Other, move |b| {
                let result = b.search_mods(&config, &id, &query, offset)?;
                let total = result.total_hits;
                let _ = b.tx.send(Message::ModSearch {
                    instance: id,
                    query,
                    result,
                    updates: false,
                });
                Ok(format!("找到 {total} 个兼容项目"))
            });
        } else {
            self.log("请先在本地版本页按 M 选择实例");
        }
    }
    fn install_online_mod(&mut self, project: String, version: Option<String>) {
        if let Some(id) = self.mod_instance.clone() {
            let config = self.config.clone();
            self.work(WorkKind::Download, move |b| {
                b.install_mod(&config, &id, &project, version.as_deref())
            });
        }
    }
    fn load_mod_versions(&mut self) {
        if let (Some(id), Some(result)) = (self.mod_instance.clone(), &self.mod_search) {
            if let Some(hit) = result.hits.get(self.selection) {
                let project = hit.project_id.clone();
                let config = self.config.clone();
                self.work(WorkKind::Other, move |b| {
                    let versions = b.mod_versions(&config, &id, &project)?;
                    let count = versions.len();
                    let _ = b.tx.send(Message::ModVersions {
                        instance: id,
                        project,
                        versions,
                    });
                    Ok(format!("{count} 个兼容版本，Enter 安装所选版本"))
                });
            }
        }
    }
    fn check_mod_updates(&mut self) {
        if let Some(id) = self.mod_instance.clone() {
            let config = self.config.clone();
            self.work(WorkKind::Other, move |b| {
                let hits = b.mod_updates(&config, &id)?;
                let total = hits.len() as u64;
                let _ = b.tx.send(Message::ModSearch {
                    instance: id,
                    query: "可用更新".into(),
                    result: crate::modrinth::SearchResult {
                        hits,
                        total_hits: total,
                        offset: 0,
                    },
                    updates: true,
                });
                Ok(format!("发现 {total} 个在线管理项目可更新，Enter 安装"))
            });
        }
    }
    fn report(&mut self, result: Result<String>) {
        match result {
            Ok(s) => self.log(s),
            Err(e) => self.log(format!("错误 · {e:#}")),
        }
    }
    fn install_loader(&mut self, loader: Loader) {
        self.choose_loader(match loader {
            Loader::Fabric => crate::loader_catalog::InstallerKind::Fabric,
            Loader::Quilt => crate::loader_catalog::InstallerKind::Quilt,
        });
    }
    fn install_forge(&mut self, kind: crate::forge::ForgeKind) {
        self.choose_loader(match kind {
            crate::forge::ForgeKind::Forge => crate::loader_catalog::InstallerKind::Forge,
            crate::forge::ForgeKind::NeoForge => crate::loader_catalog::InstallerKind::NeoForge,
        });
    }
    fn choose_loader(&mut self, kind: crate::loader_catalog::InstallerKind) {
        if let Some(mut id) = self.selected() {
            let config = self.config.clone();
            if self.tab == 0 {
                match config.root().and_then(|root| version::resolve(&root, &id)) {
                    Ok(resolved) => id = resolved.chain[0].clone(),
                    Err(e) => {
                        self.log(e.to_string());
                        return;
                    }
                }
            }
            self.work(WorkKind::Other, move |b| {
                let versions = b.loader_versions(kind, &id)?;
                let _ = b.tx.send(Message::LoaderVersions {
                    kind,
                    game: id,
                    versions,
                });
                Ok("选择加载器版本后按 Enter 安装，Esc 取消".into())
            });
        }
    }
    fn repair(&mut self) {
        if self.tab == 0 {
            if let Some(id) = self.selected() {
                let config = self.config.clone();
                self.work(WorkKind::Download, move |b| {
                    b.repair(&config, &id)?;
                    Ok(format!("{id} 已校验修复"))
                });
            }
        }
    }
    fn setting_value(&self, index: usize) -> String {
        let c = self
            .scope
            .as_ref()
            .map(|id| self.config.effective(id))
            .unwrap_or_else(|| self.config.clone());
        match index {
            0 => c.username,
            1 => c.java,
            2 => c.memory_mb.to_string(),
            3 => c.game_dir.display().to_string(),
            4 => c.isolated.to_string(),
            5 => c.jvm_args,
            6 => c.game_args,
            7 => c.download_threads.to_string(),
            8 => c.width.to_string(),
            9 => c.height.to_string(),
            10 => c.microsoft_client_id,
            11 => c.download_source,
            12 => c.optional_pack_files.to_string(),
            13 => c.fullscreen.to_string(),
            _ => c.server,
        }
    }
    fn save_setting(&mut self, value: &str) -> Result<()> {
        let mut candidate = self.config.clone();
        if let Some(id) = &self.scope {
            let instance = candidate.instances.entry(id.clone()).or_default();
            match self.setting {
                1 => instance.java = Some(value.into()),
                2 => instance.memory_mb = Some(value.parse().context("内存必须为整数 MB")?),
                4 => instance.isolated = Some(parse_bool(value)?),
                5 => instance.jvm_args = Some(value.into()),
                6 => instance.game_args = Some(value.into()),
                13 => instance.fullscreen = Some(parse_bool(value)?),
                14 => instance.server = Some(value.trim().into()),
                _ => bail!("此项为全局设置，按 G 切换到全局后修改"),
            }
            candidate.effective(id).validate()?;
        } else {
            match self.setting {
                0 => candidate.username = value.into(),
                1 => candidate.java = value.into(),
                2 => candidate.memory_mb = value.parse().context("内存必须为整数 MB")?,
                3 => candidate.game_dir = value.into(),
                4 => candidate.isolated = parse_bool(value)?,
                5 => candidate.jvm_args = value.into(),
                6 => candidate.game_args = value.into(),
                7 => candidate.download_threads = value.parse().context("线程数必须为整数")?,
                8 => candidate.width = value.parse().context("宽度必须为整数")?,
                9 => candidate.height = value.parse().context("高度必须为整数")?,
                10 => candidate.microsoft_client_id = value.trim().into(),
                11 => candidate.download_source = value.trim().into(),
                12 => candidate.optional_pack_files = parse_bool(value)?,
                13 => candidate.fullscreen = parse_bool(value)?,
                _ => candidate.server = value.trim().into(),
            }
        }
        candidate.save()?;
        self.config = candidate;
        self.local = backend::installed(&self.config);
        self.log("设置已保存");
        Ok(())
    }
    fn save_editor(&mut self) {
        let Some(editor) = self.editor.take() else {
            return;
        };
        let result = match editor.kind {
            EditKind::BackupWorld => {
                if let Some(picker) = &self.world_picker {
                    if let Some(world) = picker.worlds.get(picker.selected).cloned() {
                        let id = picker.instance.clone();
                        let config = self.config.clone();
                        let output = std::path::PathBuf::from(editor.text.trim().trim_matches('"'));
                        self.work(WorkKind::Other, move |b| {
                            b.backup_world(&config, &id, &world, &output)?;
                            Ok(format!("存档已备份：{}", output.display()))
                        });
                    }
                }
                Ok(())
            }
            EditKind::ImportWorldZip => {
                self.pending_world_zip = Some(std::path::PathBuf::from(
                    editor.text.trim().trim_matches('"'),
                ));
                self.editor = Some(Editor {
                    kind: EditKind::ImportWorldName,
                    text: "导入的世界".into(),
                });
                Ok(())
            }
            EditKind::ImportWorldName => {
                if let (Some(picker), Some(input)) =
                    (&self.world_picker, self.pending_world_zip.take())
                {
                    let id = picker.instance.clone();
                    let config = self.config.clone();
                    let name = editor.text.trim().to_owned();
                    self.work(WorkKind::Other, move |b| {
                        b.import_world(&config, &id, &input, &name)?;
                        Ok(format!("存档已导入：{name}"))
                    });
                }
                Ok(())
            }
            EditKind::ModQuery => {
                self.search_online_mods(editor.text.clone(), 0);
                Ok(())
            }
            EditKind::CurseKey => crate::curseforge::save_key(&editor.text),
            EditKind::NameInstance => self
                .editor_instance
                .as_ref()
                .context("未选择实例")
                .and_then(|id| crate::instances::set_name(&self.config, id, &editor.text)),
            EditKind::CloneInstance => {
                if let Some(id) = self.editor_instance.clone() {
                    let config = self.config.clone();
                    let target = editor.text.clone();
                    self.work(WorkKind::Other, move |b| {
                        crate::instances::clone_instance(&config, &id, &target, &b.control)?;
                        Ok(format!("实例已复制：{target}"))
                    });
                    Ok(())
                } else {
                    Err(anyhow::anyhow!("未选择实例"))
                }
            }
            EditKind::ExportPack => {
                if let Some(id) = self.editor_instance.clone() {
                    let config = self.config.clone();
                    let path = std::path::PathBuf::from(editor.text.trim_matches('"'));
                    self.work(WorkKind::Other, move |b| {
                        b.export_pack(&config, &id, &path)?;
                        Ok(format!("导出完成：{}", path.display()))
                    });
                    Ok(())
                } else {
                    Err(anyhow::anyhow!("未选择实例"))
                }
            }
            EditKind::Search => {
                self.query = editor.text.clone();
                self.selection = 0;
                Ok(())
            }
            EditKind::Setting => self.save_setting(&editor.text),
            EditKind::OfflineAccount => self.accounts.add_offline(&editor.text),
            EditKind::ImportPack => {
                let config = self.config.clone();
                let path = std::path::PathBuf::from(editor.text.trim_matches('"'));
                self.work(WorkKind::Download, move |b| {
                    let id = b.import_pack(&config, &path, None)?;
                    Ok(format!("整合包已导入：{id}"))
                });
                Ok(())
            }
            EditKind::ImportMod => self
                .mod_instance
                .as_ref()
                .context("请先在本地版本页按 M 选择实例")
                .and_then(|id| {
                    mods::import(&self.config, id, Path::new(editor.text.trim_matches('"')))
                }),
        };
        if let Err(e) = result {
            self.log(e.to_string());
            self.editor = Some(editor);
        } else if matches!(editor.kind, EditKind::CurseKey) {
            self.log("CurseForge API 密钥已保存在系统安全存储");
        } else if matches!(editor.kind, EditKind::NameInstance) {
            self.names = crate::instances::names(&self.config);
            self.log("显示名称已更新，实例 ID 与依赖关系保持稳定");
        } else if matches!(editor.kind, EditKind::ImportMod) {
            self.log("Mod 已导入");
            self.refresh_mods();
        }
    }
    fn messages(&mut self) {
        while let Ok(message) = self.rx.try_recv() {
            match message {
                Message::LoaderVersions {
                    kind,
                    game,
                    versions,
                } => {
                    self.loader_picker = Some(LoaderPicker {
                        kind,
                        game,
                        versions,
                        selected: 0,
                    });
                }
                Message::ModSearch {
                    instance,
                    query,
                    result,
                    updates,
                } => {
                    if self.mod_instance.as_ref() == Some(&instance) {
                        self.mod_updates_only = updates;
                        self.mod_query = query;
                        self.mod_search = Some(result);
                        self.mod_versions = None;
                        self.selection = 0;
                    }
                }
                Message::ModVersions {
                    instance,
                    project,
                    versions,
                } => {
                    if self.mod_instance.as_ref() == Some(&instance) {
                        self.mod_versions = Some((project, versions));
                        self.selection = 0;
                    }
                }
                Message::Pauseable(available) => {
                    self.pause_allowed = available;
                    if !available && self.paused {
                        self.control.toggle_pause();
                        self.paused = false;
                    }
                }
                Message::Log(s) => self.log(s),
                Message::Progress(s, n, total) => {
                    if self
                        .transfer
                        .as_ref()
                        .is_some_and(|(transfer, _)| transfer.label != s)
                    {
                        self.transfer = None;
                    }
                    self.progress = (s, n, total);
                }
                Message::Transfer(transfer) => {
                    if self.task.is_some() {
                        self.transfer = Some((transfer, std::time::Instant::now()));
                    }
                }
                Message::Versions(versions) => {
                    self.remote = versions;
                    self.selection = 0;
                }
                Message::Done(result) => {
                    self.task = None;
                    self.transfer = None;
                    self.paused = false;
                    self.confirm_stop = false;
                    self.progress = ("空闲".into(), 0, 0);
                    self.local = backend::installed(&self.config);
                    self.names = crate::instances::names(&self.config);
                    if let Some(picker) = &mut self.world_picker {
                        if let Ok(worlds) = crate::worlds::list(&self.config, &picker.instance) {
                            picker.worlds = worlds;
                            picker.selected =
                                picker.selected.min(picker.worlds.len().saturating_sub(1));
                        }
                    }
                    if self.mod_instance.is_some() {
                        self.refresh_mods();
                    }
                    match Accounts::load() {
                        Ok(accounts) => self.accounts = accounts,
                        Err(e) => self.log(e.to_string()),
                    }
                    match result {
                        Ok(s) => self.log(s),
                        Err(e) => self.log(format!("错误 · {e}")),
                    }
                }
            }
        }
        if self.tab != 2 {
            self.selection = self.selection.min(self.count().saturating_sub(1));
        }
    }
    fn key(&mut self, key: event::KeyEvent) -> bool {
        if key.kind != KeyEventKind::Press {
            return false;
        }
        if let Some(id) = self.confirm_remove.clone() {
            match key.code {
                KeyCode::Char('y') => {
                    let result = crate::instances::trash(&self.config, &id);
                    self.report(
                        result
                            .map(|entry| format!("实例已移至回收站，可按 6 恢复：{}", entry.label)),
                    );
                    self.confirm_remove = None;
                    self.local = backend::installed(&self.config);
                    self.names = crate::instances::names(&self.config);
                }
                KeyCode::Esc | KeyCode::Char('n') => self.confirm_remove = None,
                _ => {}
            }
            return false;
        }
        if self.help {
            if matches!(key.code, KeyCode::Esc | KeyCode::F(1) | KeyCode::Char('q')) {
                self.help = false;
                self.help_scroll = 0;
            } else {
                match key.code {
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.help_scroll = (self.help_scroll + 1).min(16)
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.help_scroll = self.help_scroll.saturating_sub(1)
                    }
                    KeyCode::Home => self.help_scroll = 0,
                    _ => {}
                }
            }
            return false;
        }
        if self.confirm_stop {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.control.cancel();
                    self.confirm_stop = false;
                    self.log("正在终止游戏…");
                }
                KeyCode::Esc | KeyCode::Char('n') => self.confirm_stop = false,
                _ => {}
            }
            return false;
        }
        if self.editor.is_some() {
            match key.code {
                KeyCode::Esc => self.editor = None,
                KeyCode::Enter => self.save_editor(),
                KeyCode::Backspace => {
                    self.editor.as_mut().unwrap().text.pop();
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.editor.as_mut().unwrap().text.clear()
                }
                KeyCode::Char(c) => self.editor.as_mut().unwrap().text.push(c),
                _ => {}
            }
            return false;
        }
        if let Some(picker) = &mut self.world_picker {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') if self.task.is_none() => {
                    self.world_picker = None
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    picker.selected = picker.selected.saturating_sub(1)
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    picker.selected =
                        (picker.selected + 1).min(picker.worlds.len().saturating_sub(1))
                }
                KeyCode::Home => picker.selected = 0,
                KeyCode::End => picker.selected = picker.worlds.len().saturating_sub(1),
                KeyCode::PageUp => picker.selected = picker.selected.saturating_sub(10),
                KeyCode::PageDown => {
                    picker.selected =
                        (picker.selected + 10).min(picker.worlds.len().saturating_sub(1))
                }
                KeyCode::Enter | KeyCode::Char('b') if self.task.is_none() => {
                    if let Some(world) = picker.worlds.get(picker.selected) {
                        let stamp = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs();
                        self.editor = Some(Editor {
                            kind: EditKind::BackupWorld,
                            text: format!("{world}-{stamp}.zip"),
                        });
                    }
                }
                KeyCode::Char('i') if self.task.is_none() => {
                    self.editor = Some(Editor {
                        kind: EditKind::ImportWorldZip,
                        text: String::new(),
                    });
                }
                KeyCode::Char('r') if self.task.is_none() => {
                    match crate::worlds::list(&self.config, &picker.instance) {
                        Ok(worlds) => {
                            picker.worlds = worlds;
                            picker.selected = 0;
                        }
                        Err(error) => self.log(format!("读取存档失败：{error}")),
                    }
                }
                KeyCode::Char('c') if self.task.is_some() => self.control.cancel(),
                _ => (),
            }
            return false;
        }
        if let Some(picker) = &mut self.loader_picker {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => self.loader_picker = None,
                KeyCode::Up | KeyCode::Char('k') => {
                    picker.selected = picker.selected.saturating_sub(1)
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    picker.selected = (picker.selected + 1).min(picker.versions.len())
                }
                KeyCode::Home => picker.selected = 0,
                KeyCode::End => picker.selected = picker.versions.len(),
                KeyCode::PageUp => picker.selected = picker.selected.saturating_sub(10),
                KeyCode::PageDown => {
                    picker.selected = (picker.selected + 10).min(picker.versions.len())
                }
                KeyCode::Enter if self.task.is_none() => {
                    let picker = self.loader_picker.take().unwrap();
                    let requested = picker
                        .selected
                        .checked_sub(1)
                        .map(|i| picker.versions[i].version.clone());
                    let config = self.config.clone();
                    self.work(WorkKind::Download, move |b| {
                        let instance = b.install_selected_loader(
                            &config,
                            picker.kind,
                            &picker.game,
                            requested.as_deref(),
                        )?;
                        Ok(format!("安装完成：{instance}，请前往本地版本启动"))
                    });
                }
                _ => (),
            }
            return false;
        }
        match key.code {
            KeyCode::Char('w') if self.tab == 0 && self.task.is_none() => {
                if let Some(id) = self.selected() {
                    match crate::worlds::list(&self.config, &id) {
                        Ok(worlds) => {
                            self.world_picker = Some(WorldPicker {
                                instance: id,
                                worlds,
                                selected: 0,
                            })
                        }
                        Err(error) => self.log(format!("读取存档失败：{error}")),
                    }
                }
            }
            KeyCode::F(1) => self.help = true,
            KeyCode::Char('q') | KeyCode::Esc => {
                if self.task.is_none() {
                    return true;
                } else {
                    self.log("任务正在运行；C 取消任务，游戏终止需再按 Y 确认");
                }
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.task.is_none() {
                    return true;
                } else {
                    self.log("请按 C 取消任务后退出");
                }
            }
            KeyCode::Right if self.tab == 6 => {
                self.log_horizontal = self.log_horizontal.saturating_add(8);
            }
            KeyCode::Left if self.tab == 6 => {
                self.log_horizontal = self.log_horizontal.saturating_sub(8);
            }
            KeyCode::Tab | KeyCode::Right => {
                self.tab = (self.tab + 1) % 7;
                self.selection = 0;
                if self.tab == 5 {
                    self.refresh_trash();
                }
            }
            KeyCode::BackTab | KeyCode::Left => {
                self.tab = (self.tab + 6) % 7;
                self.selection = 0;
                if self.tab == 5 {
                    self.refresh_trash();
                }
            }
            KeyCode::Char(c @ '1'..='7') => {
                self.tab = c as usize - '1' as usize;
                self.selection = 0;
                if self.tab == 5 {
                    self.refresh_trash();
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.tab == 2 {
                    self.setting = (self.setting + 1) % SETTINGS.len();
                } else if self.count() > 0 {
                    self.selection = (self.selection + 1) % self.count();
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.tab == 2 {
                    self.setting = (self.setting + SETTINGS.len() - 1) % SETTINGS.len();
                } else if self.count() > 0 {
                    self.selection = (self.selection + self.count() - 1) % self.count();
                }
            }
            KeyCode::PageUp if self.tab == 6 => {
                self.log_scroll = (self.log_scroll + 10).min(self.logs.len().saturating_sub(1))
            }
            KeyCode::PageDown if self.tab == 6 => {
                self.log_scroll = self.log_scroll.saturating_sub(10)
            }
            KeyCode::Home if self.tab == 6 => self.log_scroll = self.logs.len().saturating_sub(1),
            KeyCode::End if self.tab == 6 => self.log_scroll = 0,
            KeyCode::Home | KeyCode::End | KeyCode::PageUp | KeyCode::PageDown => {
                let last = self.count().saturating_sub(1);
                let selected = if self.tab == 2 {
                    &mut self.setting
                } else {
                    &mut self.selection
                };
                *selected = match key.code {
                    KeyCode::Home => 0,
                    KeyCode::End => last,
                    KeyCode::PageUp => selected.saturating_sub(10),
                    _ => selected.saturating_add(10).min(last),
                };
            }
            KeyCode::Enter => self.action(),
            KeyCode::Char('r') => self.refresh(),
            KeyCode::Char('x') => self.repair(),
            KeyCode::F(2) if self.tab == 0 && self.task.is_none() => {
                if let Some(id) = self.selected() {
                    self.editor_instance = Some(id.clone());
                    self.editor = Some(Editor {
                        kind: EditKind::NameInstance,
                        text: self.names.get(&id).cloned().unwrap_or(id),
                    });
                }
            }
            KeyCode::Char('t') if self.tab == 0 && self.task.is_none() => {
                if let Some(id) = self.selected() {
                    self.editor_instance = Some(id.clone());
                    self.editor = Some(Editor {
                        kind: EditKind::CloneInstance,
                        text: format!("{id}-copy"),
                    });
                }
            }
            KeyCode::Char('e') if self.tab == 0 && self.task.is_none() => {
                if let Some(id) = self.selected() {
                    self.editor_instance = Some(id.clone());
                    self.editor = Some(Editor {
                        kind: EditKind::ExportPack,
                        text: format!("{id}.mrpack"),
                    });
                }
            }
            KeyCode::Delete if self.tab == 0 && self.task.is_none() => {
                self.confirm_remove = self.selected()
            }
            KeyCode::Char('z')
                if self.tab == 3
                    && self.task.is_none()
                    && self.mod_search.is_none()
                    && self.mod_versions.is_none() =>
            {
                if let Some(id) = &self.mod_instance {
                    let result = mods::removed(&self.config, id).and_then(|entries| {
                        let entry = entries.first().context("没有可恢复的 Mod 备份")?;
                        mods::restore(&self.config, id, &entry.token)
                    });
                    self.report(result.map(|filename| format!("已恢复 {filename}")));
                    self.refresh_mods();
                }
            }
            KeyCode::Delete
                if self.tab == 3
                    && self.task.is_none()
                    && self.mod_search.is_none()
                    && self.mod_versions.is_none() =>
            {
                if let (Some(id), Some(entry)) =
                    (&self.mod_instance, self.mod_list.get(self.selection))
                {
                    let result = mods::remove(&self.config, id, &entry.filename)
                        .map(|backup| format!("Mod 已移出，备份：{}", backup.display()));
                    self.report(result);
                    self.refresh_mods();
                }
            }
            KeyCode::Char('l') if self.tab == 0 => {
                if let Some(id) = self.selected() {
                    let result = self
                        .config
                        .effective(&id)
                        .instance_dir(&id)
                        .and_then(|dir| crate::logs::recent(&dir.join("tui-mc-game.log")));
                    match result {
                        Ok(lines) => {
                            for line in lines {
                                self.log(format!("游戏 · {line}"));
                            }
                            self.tab = 6;
                            self.log_scroll = 0;
                        }
                        Err(e) => self.log(format!("读取日志失败：{e}")),
                    }
                }
            }
            KeyCode::Char('d') if self.tab == 0 && self.task.is_none() => {
                if let Some(id) = self.selected() {
                    match crate::diagnostics::analyze(&self.config, &id, None) {
                        Ok(diagnosis) => {
                            for line in diagnosis.summary().lines() {
                                self.log(line);
                            }
                            self.tab = 6;
                            self.log_scroll = 0;
                        }
                        Err(error) => self.log(format!("诊断失败：{error}")),
                    }
                }
            }
            KeyCode::Char('b') if self.tab == 0 => {
                if let Some(id) = self.selected() {
                    let config = self.config.clone();
                    self.work(WorkKind::Download, move |b| {
                        let runtime = b.install_java_for_version(&config, &id)?;
                        Ok(format!(
                            "Java {} 已就绪：{}",
                            runtime.major,
                            runtime.path.display()
                        ))
                    });
                }
            }
            KeyCode::Char('s') if self.tab == 1 => {
                self.snapshots = !self.snapshots;
                self.selection = 0;
            }
            KeyCode::Char('/') if self.tab < 2 => {
                self.editor = Some(Editor {
                    kind: EditKind::Search,
                    text: self.query.clone(),
                })
            }
            KeyCode::Char('b' | '/') if self.tab == 3 && self.task.is_none() => {
                self.editor = Some(Editor {
                    kind: EditKind::ModQuery,
                    text: self.mod_query.clone(),
                })
            }
            KeyCode::Char('l') if self.tab == 3 => {
                self.mod_search = None;
                self.mod_versions = None;
                self.selection = 0;
                self.refresh_mods();
            }
            KeyCode::Backspace if self.tab == 3 => {
                if self.mod_versions.is_some() {
                    self.mod_versions = None;
                } else {
                    self.mod_search = None;
                }
                self.selection = 0;
            }
            KeyCode::Char('v')
                if self.tab == 3 && self.mod_search.is_some() && self.task.is_none() =>
            {
                self.load_mod_versions()
            }
            KeyCode::Char('u') if self.tab == 3 && self.task.is_none() => self.check_mod_updates(),
            KeyCode::Char('n')
                if self.tab == 3 && self.task.is_none() && !self.mod_updates_only =>
            {
                if let Some(result) = &self.mod_search {
                    let offset = result.offset.saturating_add(20);
                    if offset < result.total_hits {
                        self.search_online_mods(self.mod_query.clone(), offset);
                    }
                }
            }
            KeyCode::Char('p')
                if self.tab == 3 && self.task.is_none() && !self.mod_updates_only =>
            {
                if let Some(result) = &self.mod_search {
                    self.search_online_mods(
                        self.mod_query.clone(),
                        result.offset.saturating_sub(20),
                    );
                }
            }
            KeyCode::Char('f') if self.tab < 2 => self.install_loader(Loader::Fabric),
            KeyCode::Char('u') if self.tab < 2 => self.install_loader(Loader::Quilt),
            KeyCode::Char('h') if self.tab < 2 => {
                self.install_forge(crate::forge::ForgeKind::Forge)
            }
            KeyCode::Char('n') if self.tab < 2 => {
                self.install_forge(crate::forge::ForgeKind::NeoForge)
            }
            KeyCode::Char('o') if self.tab == 0 => {
                if let Some(id) = self.selected() {
                    self.scope = Some(id);
                    self.tab = 2;
                    self.setting = 1;
                }
            }
            KeyCode::Char('g') if self.tab == 2 => {
                self.scope = None;
                self.log("已切换为全局设置");
            }
            KeyCode::Char('d') if self.tab == 2 && self.task.is_none() => {
                if let Some(id) = &self.scope {
                    self.config.instances.remove(id);
                    let result = crate::instances::clear_settings(&self.config, id)
                        .and_then(|_| self.config.save());
                    self.report(result.map(|_| "实例已恢复继承全局设置".into()));
                }
            }
            KeyCode::Char('m') if self.tab == 0 => {
                if let Some(id) = self.selected() {
                    self.mod_instance = Some(id);
                    self.mod_search = None;
                    self.mod_versions = None;
                    self.mod_updates_only = false;
                    self.tab = 3;
                    self.selection = 0;
                    self.refresh_mods();
                }
            }
            KeyCode::Char('a') if self.tab == 3 && self.task.is_none() => {
                self.editor = Some(Editor {
                    kind: EditKind::ImportMod,
                    text: String::new(),
                })
            }
            KeyCode::Char('a') if self.tab == 4 && self.task.is_none() => {
                let config = self.config.clone();
                self.work(WorkKind::Other, move |b| b.login_microsoft(&config));
                self.tab = 6;
            }
            KeyCode::Char('K') if self.tab == 4 && self.task.is_none() => {
                self.editor = Some(Editor {
                    kind: EditKind::CurseKey,
                    text: String::new(),
                })
            }
            KeyCode::Char('i') if self.tab < 2 && self.task.is_none() => {
                self.editor = Some(Editor {
                    kind: EditKind::ImportPack,
                    text: String::new(),
                })
            }
            KeyCode::Char('n') if self.tab == 4 && self.task.is_none() => {
                self.editor = Some(Editor {
                    kind: EditKind::OfflineAccount,
                    text: self.config.username.clone(),
                })
            }
            KeyCode::Char('d') if self.tab == 4 && self.task.is_none() => {
                if let Some(account) = self.accounts.entries.get(self.selection) {
                    let id = account.id.clone();
                    let result = self.accounts.remove(&id);
                    self.report(result.map(|_| "账户与本机凭证已移除".into()));
                }
            }
            KeyCode::Char('v') => {
                let config = self.config.clone();
                self.work(WorkKind::Other, move |b| {
                    let found = java::discover(&config);
                    for runtime in &found {
                        b.log(format!(
                            "Java {} · {}",
                            runtime.major,
                            runtime.path.display()
                        ));
                    }
                    if found.is_empty() {
                        bail!("没有发现可用 Java");
                    }
                    Ok(format!(
                        "发现 {} 个 Java；设置 auto 可自动匹配",
                        found.len()
                    ))
                });
            }
            KeyCode::Char('c') => {
                if let Some(kind) = self.task {
                    if kind == WorkKind::Game {
                        self.confirm_stop = true;
                    } else {
                        self.control.cancel();
                        self.log("正在取消，等待当前网络读取返回…");
                    }
                }
            }
            KeyCode::Char('p') if self.task == Some(WorkKind::Download) && self.pause_allowed => {
                self.paused = self.control.toggle_pause();
                self.log(if self.paused {
                    "下载已请求暂停；P 继续"
                } else {
                    "下载继续"
                });
            }
            _ => {}
        }
        false
    }
}
fn parse_bool(value: &str) -> Result<bool> {
    match value.trim() {
        "true" | "是" | "1" => Ok(true),
        "false" | "否" | "0" => Ok(false),
        _ => bail!("请输入 true 或 false"),
    }
}
struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}
pub fn run(config: Config) -> Result<()> {
    enable_raw_mode()?;
    let _guard = TerminalGuard;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let (tx, rx) = mpsc::channel();
    let mut app = App::new(config, tx, rx);
    app.log("欢迎使用 Tui-Launcher。/ 搜索 · F Fabric · U Quilt · O 实例设置 · M 管理 Mod\n W 存档备份 / 导入");
    app.log("5 账户管理 · 微软登录需先在设置中配置自己的 OAuth 客户端 ID。");
    loop {
        app.messages();
        terminal.draw(|f| draw(f, &app))?;
        if event::poll(Duration::from_millis(80))? {
            if let Event::Key(key) = event::read()? {
                if app.key(key) {
                    break;
                }
            }
        }
    }
    Ok(())
}
fn panel(title: impl Into<String>) -> Block<'static> {
    Block::default()
        .title(format!(" {} ", title.into()))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Rgb(48, 65, 84)))
}
fn highlight() -> Style {
    Style::default()
        .bg(Color::Rgb(30, 57, 56))
        .fg(ACCENT)
        .add_modifier(Modifier::BOLD)
}
fn render_list(f: &mut Frame, items: Vec<ListItem>, title: String, selected: usize, area: Rect) {
    if items.is_empty() {
        f.render_widget(
            Paragraph::new("\n  暂无内容\n\n  F1 查看当前页操作")
                .style(Style::default().fg(MUTED))
                .block(panel(title)),
            area,
        );
        return;
    }
    let mut state = ListState::default();
    let selected = selected.min(items.len() - 1);
    state.select(Some(selected));
    let position = format!(" {}/{} ", selected + 1, items.len());
    f.render_stateful_widget(
        List::new(items)
            .block(panel(title).title_bottom(Line::from(position).right_aligned()))
            .highlight_style(highlight())
            .highlight_symbol("▸ "),
        area,
        &mut state,
    );
}
fn dialog_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2));
    let height = height.min(area.height.saturating_sub(2));
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

fn page_actions(app: &App) -> &'static str {
    match app.tab {
        0 => "Enter 启动 · X 修复 · O 设置 · M Mod\nW 存档 · B Java · L 日志 · D 诊断\nF Fabric · U Quilt · H Forge · N NeoForge\nF2 名称 · T 复制 · E 导出 · Del 回收\nI 导入整合包 · / 搜索 · R 刷新",
        1 => "Enter 安装原版 · R 获取版本目录\nF Fabric · U Quilt · H Forge · N NeoForge\nS 切换快照 · / 搜索 · I 导入整合包",
        6 => "PgUp / PgDn 翻页 · ← → 查看长行\nHome 最早记录 · End 跟随最新记录",
        2 => "Enter 编辑 · G 全局设置\nD 清除当前实例覆盖 · V 检测 Java\n编辑：Enter 保存 · Esc 取消 · Ctrl+U 清空",
        3 if app.mod_versions.is_some() => "Enter 安装所选版本\nBackspace 返回搜索结果 · L 本地 Mod",
        3 if app.mod_search.is_some() => "Enter 安装 · V 选择版本 · B 搜索\nN 下一页 · P 上一页 · L 本地 Mod",
        3 => "Enter 启停 · A 导入 JAR · B 在线搜索\nU 检查更新 · Del 移出 · Z 恢复 · R 刷新\n在本地页按 M 选择要管理的实例",
        4 => "Enter 切换账户 · N 添加离线账户\nA 微软登录 · D 移除账户\nShift+K 配置 CurseForge 密钥\n微软登录：先在设置中填写 OAuth 应用 ID",
        _ => "Enter 恢复实例 · R 刷新\n本地页按 Delete 将实例移入回收站",
    }
}

// Keep the input tail visible using terminal display columns, including CJK.
fn input_tail(text: &str, width: u16) -> String {
    let mut used = 0;
    let mut tail = Vec::new();
    for c in text.chars().rev() {
        let cell_width = Line::from(c.to_string()).width();
        if used + cell_width > usize::from(width) {
            break;
        }
        tail.push(c);
        used += cell_width;
    }
    tail.into_iter().rev().collect()
}

fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    f.render_widget(
        Block::default().style(Style::default().bg(BG).fg(TEXT)),
        area,
    );
    if area.width < 48 || area.height < 16 {
        f.render_widget(
            Paragraph::new("请将终端调整至至少 48 × 16\nQ 空闲时退出")
                .style(Style::default().fg(ACCENT))
                .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }
    let rows = Layout::default()
        .margin(1)
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(if area.height >= 24 { 3 } else { 1 }),
            Constraint::Length(1),
            Constraint::Min(4),
            Constraint::Length(if app.task.is_some() { 3 } else { 1 }),
            Constraint::Length(2),
        ])
        .split(area);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " Tui-Launcher ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  JAVA EDITION  /  {}", env!("CARGO_PKG_VERSION")),
                Style::default().fg(MUTED),
            ),
        ]))
        .block(if area.height >= 24 {
            panel("方块之间 · 即刻出发")
        } else {
            Block::default()
        }),
        rows[0],
    );
    let tabs = ["1本地", "2下载", "3设置", "4Mod", "5账户", "6回收", "7日志"];
    f.render_widget(
        Paragraph::new(Line::from(
            tabs.iter()
                .enumerate()
                .map(|(i, t)| {
                    Span::styled(
                        format!("{t} "),
                        Style::default()
                            .fg(if app.tab == i { BG } else { MUTED })
                            .bg(if app.tab == i { ACCENT } else { BG }),
                    )
                })
                .collect::<Vec<_>>(),
        )),
        rows[1],
    );
    let cols = if area.width >= 100 {
        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(48),
                Constraint::Length((area.width / 3).clamp(36, 48)),
            ])
            .split(rows[2]);
        [split[0], split[1]]
    } else {
        [rows[2], Rect::default()]
    };
    match app.tab {
        0 | 1 => {
            let items = if app.tab == 0 {
                app.local_visible()
                    .into_iter()
                    .map(|id| {
                        ListItem::new(match app.names.get(id) {
                            Some(name) => format!(" ◇ {name}  [{id}]"),
                            None => format!(" ◇ {id}"),
                        })
                    })
                    .collect::<Vec<_>>()
            } else {
                app.remote_visible()
                    .into_iter()
                    .map(|v| ListItem::new(format!(" ↓ {:<24} {}", v.id, v.kind)))
                    .collect()
            };
            let title = format!(
                "{} · {}{}",
                if app.tab == 0 {
                    "本地游戏"
                } else {
                    "官方版本"
                },
                items.len(),
                if app.query.is_empty() {
                    String::new()
                } else {
                    format!(" · /{}", app.query)
                }
            );
            if items.is_empty() {
                f.render_widget(Paragraph::new(if !app.query.is_empty(){"\n 没有匹配的版本。\n 按 /，Ctrl+U 清空搜索后 Enter。"}else if app.tab==0{"\n 还没有游戏版本。\n 按 2 前往下载中心，R 获取官方列表。\n 也可在设置中指定已有游戏目录。"}else{"\n R 获取 Mojang 官方版本目录。\n Enter 原版 · F Fabric · U Quilt\n S 显示快照 · / 搜索"}).block(panel(title)).wrap(Wrap{trim:false}),cols[0]);
            } else {
                render_list(f, items, title, app.selection, cols[0]);
            }
            let selected = app.selected().unwrap_or_else(|| "尚未选择".into());
            let config = app.config.effective(&selected);
            let help = if app.tab == 0 {
                "Enter 启动游戏\n X 检查并修复\n O 实例设置 · M 管理 Mod\n W 存档 · F1 更多操作"
            } else {
                "Enter 安装原版\n F Fabric · U Quilt\n H Forge · N NeoForge\n S 快照 · / 搜索\n F1 更多操作"
            };
            f.render_widget(
                Paragraph::new(format!(
                    "{selected}\n\n{}\n{} MB · {}\n\n{help}",
                    app.accounts
                        .active()
                        .map(|a| format!(
                            "{} · {}",
                            a.name,
                            if a.kind == AccountKind::Microsoft {
                                "微软"
                            } else {
                                "离线"
                            }
                        ))
                        .unwrap_or_else(|| format!("{} · 离线", config.username)),
                    config.memory_mb,
                    if config.isolated {
                        "独立实例目录"
                    } else {
                        "共享游戏目录"
                    }
                ))
                .block(panel("当前选择"))
                .wrap(Wrap { trim: false }),
                cols[1],
            );
        }
        6 => {
            let height = rows[2].height.saturating_sub(2) as usize;
            let end = app.logs.len().saturating_sub(app.log_scroll);
            let start = end.saturating_sub(height);
            let lines = app
                .logs
                .iter()
                .skip(start)
                .take(end - start)
                .map(|line| {
                    let color = if line.contains("失败") || line.contains("错误") {
                        Color::Rgb(255, 140, 140)
                    } else if line.contains("完成") || line.contains("已就绪") {
                        ACCENT
                    } else {
                        TEXT
                    };
                    Line::styled(line.as_str(), Style::default().fg(color))
                })
                .collect::<Vec<_>>();
            f.render_widget(
                Paragraph::new(lines).scroll((0, app.log_horizontal)).block(
                    panel("任务日志").title_bottom(format!(
                        " {}–{} / {} · {} ",
                        if end == 0 { 0 } else { start + 1 },
                        end,
                        app.logs.len(),
                        if app.log_scroll == 0 {
                            "跟随最新"
                        } else {
                            "历史记录"
                        }
                    )),
                ),
                rows[2],
            );
        }
        2 => {
            let items = SETTINGS
                .iter()
                .enumerate()
                .map(|(i, label)| {
                    ListItem::new(vec![
                        Line::from(Span::styled(*label, Style::default().fg(MUTED))),
                        Line::from(format!("  {}", app.setting_value(i))),
                    ])
                })
                .collect();
            let scope = app
                .scope
                .as_ref()
                .map(|id| format!("实例：{id}"))
                .unwrap_or_else(|| "全局".into());
            render_list(
                f,
                items,
                format!("启动设置 · {scope}"),
                app.setting,
                cols[0],
            );
            f.render_widget(Paragraph::new(format!("\n Enter 编辑 / 保存\n Ctrl+U 清空输入\n Esc 取消编辑\n\n G 全局设置\n D 清除实例覆盖\n V 检测 Java\n\n 实例可覆盖 Java、内存、\n 隔离与附加参数。\n\n {}",app.logs.back().map(String::as_str).unwrap_or(""))).block(panel("说明")).wrap(Wrap{trim:false}),cols[1]);
        }
        3 if app.mod_versions.is_some() => {
            let (project, versions) = app.mod_versions.as_ref().unwrap();
            let items = versions
                .iter()
                .map(|version| {
                    ListItem::new(format!(
                        " {} · {} · {}",
                        version.version_number,
                        version.version_type,
                        version.date_published.get(..10).unwrap_or("")
                    ))
                })
                .collect();
            render_list(f, items, "选择兼容 Mod 版本".into(), app.selection, cols[0]);
            let selected = versions
                .get(app.selection)
                .map(|v| v.name.as_str())
                .unwrap_or("没有兼容版本");
            f.render_widget(Paragraph::new(format!("\n {selected}\n {project}\n\n Enter 安装所选版本\n Backspace 返回项目\n L 本地 Mod 列表\n\n 自动处理必需依赖，\n 安装前保留旧文件备份。\n\n {}",app.logs.back().map(String::as_str).unwrap_or(""))).block(panel("版本安装")).wrap(Wrap{trim:false}),cols[1]);
        }
        3 if app.mod_search.is_some() => {
            let result = app.mod_search.as_ref().unwrap();
            let items = result
                .hits
                .iter()
                .map(|hit| ListItem::new(format!(" {} · {}", hit.title, hit.slug)))
                .collect();
            let title = format!(
                "在线 Mod · {} · {} / {}",
                app.mod_query,
                result.offset.saturating_add(1),
                result.total_hits
            );
            render_list(f, items, title, app.selection, cols[0]);
            let description = result
                .hits
                .get(app.selection)
                .map(|hit| hit.description.as_str())
                .unwrap_or("没有匹配项目，按 B 修改关键词");
            f.render_widget(Paragraph::new(format!("\n {description}\n\n Enter 安装兼容稳定版\n V 选择具体版本\n B / / 搜索\n N / P 下一页 / 上一页\n U 检查已管理 Mod 更新\n L 返回本地 Mod\n\n {}",app.logs.back().map(String::as_str).unwrap_or(""))).block(panel("Modrinth")).wrap(Wrap{trim:false}),cols[1]);
        }
        3 => {
            let items = app
                .mod_list
                .iter()
                .map(|entry| {
                    ListItem::new(format!(
                        " {} {:>5} KB  {}",
                        if entry.enabled { "●" } else { "○" },
                        entry.size / 1024,
                        entry.filename
                    ))
                })
                .collect();
            render_list(
                f,
                items,
                format!(
                    "Mod 管理 · {}",
                    app.mod_instance.as_deref().unwrap_or("请先选择实例")
                ),
                app.selection,
                cols[0],
            );
            f.render_widget(Paragraph::new("\n Enter 启用 / 禁用\n Del 移出并备份\n Z 恢复最近移除项\n A 导入本地 JAR\n B / / 在线搜索\n U 检查更新 · L 本地列表\n R 刷新\n\n 在本地版本页按 M 选择实例。\n\n 禁用使用 .jar.disabled 后缀，\n 原文件内容保持完整。\n\n Mod 变更在下次启动时生效。").block(panel("管理")).wrap(Wrap{trim:false}),cols[1]);
        }
        4 => {
            let items = app
                .accounts
                .entries
                .iter()
                .map(|account| {
                    ListItem::new(format!(
                        " {} {} · {}",
                        if app.accounts.active.as_ref() == Some(&account.id) {
                            "●"
                        } else {
                            "○"
                        },
                        account.name,
                        if account.kind == AccountKind::Microsoft {
                            "微软正版"
                        } else {
                            "离线"
                        }
                    ))
                })
                .collect();
            render_list(f, items, "账户管理".into(), app.selection, cols[0]);
            f.render_widget(Paragraph::new(format!("\n Enter 选择账户\n N 添加离线账户\n A 微软设备代码登录\n Shift+K 配置 CurseForge 密钥\n D 移除账户与本机凭证\n\n 微软凭证保存在系统安全存储。\n 在设置中填写自己的应用 ID\n 后即可开始登录。\n\n {}",app.logs.back().map(String::as_str).unwrap_or(""))).block(panel("登录")).wrap(Wrap{trim:false}),cols[1]);
        }
        _ => {
            let items = app
                .trash
                .iter()
                .map(|entry| ListItem::new(format!(" {} · {}", entry.label, entry.id)))
                .collect();
            render_list(f, items, "实例回收站".into(), app.selection, cols[0]);
            f.render_widget(Paragraph::new(format!("\n Enter 恢复选择的实例\n R 刷新\n\n 同名实例存在时不会覆盖。\n 回收站保留实例目录的数据。\n\n 在本地页按 Delete 移入。\n 有其他实例依赖的父版本\n 不能直接移除。\n\n {}",app.logs.back().map(String::as_str).unwrap_or(""))).block(panel("恢复")).wrap(Wrap{trim:false}),cols[1]);
        }
    }
    let (label, done, total) = &app.progress;
    let ratio = if let Some((transfer, _)) = &app.transfer {
        transfer
            .total
            .filter(|total| *total > 0)
            .map(|total| (transfer.done as f64 / total as f64).clamp(0.0, 1.0))
            .unwrap_or(0.0)
    } else if *total == 0 {
        0.0
    } else {
        (*done as f64 / *total as f64).clamp(0.0, 1.0)
    };
    let status = if app.paused {
        "已暂停 · P 继续 / C 取消"
    } else if app.task.is_some() && app.pause_allowed {
        "进行中 · P 暂停下载 / C 取消"
    } else if app.task.is_some() {
        "进行中 · C 取消"
    } else {
        "就绪"
    };
    if app.task.is_some() {
        f.render_widget(
            Gauge::default()
                .block(panel(status))
                .gauge_style(Style::default().fg(ACCENT).bg(Color::Rgb(27, 39, 50)))
                .ratio(ratio)
                .label(if app.task.is_some() {
                    if let Some((transfer, updated)) = &app.transfer {
                        if !app.paused && updated.elapsed() > Duration::from_secs(3) {
                            format!("{} · 等待数据或校验中", transfer.label)
                        } else {
                            transfer.display(app.paused)
                        }
                    } else {
                        format!("{label} {done}/{total}")
                    }
                } else {
                    "等待下一次冒险".into()
                }),
            rows[3],
        );
    } else {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" ● 就绪  ", Style::default().fg(ACCENT)),
                Span::raw(
                    app.logs
                        .back()
                        .map(String::as_str)
                        .unwrap_or("选择游戏，开始冒险"),
                ),
            ])),
            rows[3],
        );
    }
    let primary = page_actions(app).lines().next().unwrap_or("");
    f.render_widget(
        Paragraph::new(vec![
            Line::styled(primary, Style::default().fg(ACCENT)),
            Line::styled(
                "Tab 页面 · ↑↓ 选择 · F1 帮助 · Q 退出",
                Style::default().fg(MUTED),
            ),
        ]),
        rows[4],
    );
    if let Some(picker) = &app.world_picker {
        let dialog = dialog_rect(area, 86, 24);
        f.render_widget(Clear, dialog);
        let mut state = ListState::default().with_selected(Some(picker.selected));
        let items: Vec<_> = if picker.worlds.is_empty() {
            vec![ListItem::new("暂无存档，按 I 导入 ZIP")]
        } else {
            picker
                .worlds
                .iter()
                .map(|name| ListItem::new(name.as_str()))
                .collect()
        };
        f.render_stateful_widget(
            List::new(items)
                .block(
                    panel(format!("{} · 存档", picker.instance))
                        .title_bottom(" Enter 备份 · I 导入 · R 刷新 · Esc 返回 "),
                )
                .style(Style::default().bg(BG).fg(TEXT))
                .highlight_style(Style::default().bg(ACCENT).fg(BG)),
            dialog,
            &mut state,
        );
    }
    if let Some(picker) = &app.loader_picker {
        let dialog = dialog_rect(area, 86, 24);
        f.render_widget(Clear, dialog);
        let mut items = vec![ListItem::new("自动选择推荐 / 稳定版本")];
        items.extend(picker.versions.iter().map(|entry| {
            ListItem::new(format!(
                "{}{}",
                entry.version,
                if entry.stable { "" } else { "  [预览]" }
            ))
        }));
        let mut state = ListState::default().with_selected(Some(picker.selected));
        f.render_stateful_widget(
            List::new(items)
                .block(
                    panel(format!(
                        "{} / Minecraft {}",
                        picker.kind.name(),
                        picker.game
                    ))
                    .title_bottom(" ↑↓ / PgUp / PgDn 选择 · Enter 安装 · Esc 返回 "),
                )
                .style(Style::default().bg(BG).fg(TEXT))
                .highlight_style(Style::default().bg(ACCENT).fg(BG)),
            dialog,
            &mut state,
        );
    }
    if let Some(editor) = &app.editor {
        let dialog = dialog_rect(area, 88, 7);
        f.render_widget(Clear, dialog);
        let title = match editor.kind {
            EditKind::BackupWorld => "备份 ZIP 路径 · 不覆盖已有文件 · Enter 备份",
            EditKind::ImportWorldZip => "输入世界 ZIP 路径 · Enter 下一步",
            EditKind::ImportWorldName => "新存档目录名 · 不覆盖现有世界 · Enter 导入",
            EditKind::Search => "搜索 · Enter 应用 · Ctrl+U 清空",
            EditKind::Setting => "编辑设置 · Enter 保存 · Esc 取消",
            EditKind::ImportMod => "输入本地 JAR 路径 · Enter 导入",
            EditKind::OfflineAccount => "输入离线用户名 · Enter 添加并选择",
            EditKind::ImportPack => "输入 Modrinth .mrpack / CurseForge ZIP 路径",
            EditKind::NameInstance => "实例显示名称 · Enter 保存 · ID 保持稳定",
            EditKind::CloneInstance => "新实例 ID · 复制配置、Mod 与存档",
            EditKind::ExportPack => "输出 .mrpack 路径 · 不包含存档与账户",
            EditKind::CurseKey => "输入自己的 CurseForge API 密钥 · 系统安全存储",
            EditKind::ModQuery => "Modrinth 搜索关键词 · 按当前实例版本与加载器筛选",
        };
        let display = if matches!(editor.kind, EditKind::CurseKey) {
            "•".repeat(editor.text.chars().count())
        } else {
            editor.text.replace(['\n', '\r', '\t'], " ")
        };
        let visible = input_tail(&display, dialog.width.saturating_sub(6));
        let heading = title.split(" · ").next().unwrap_or(title);
        f.render_widget(
            Paragraph::new(vec![
                Line::styled(title, Style::default().fg(MUTED)),
                Line::raw(""),
                Line::from(vec![
                    Span::raw(" › "),
                    Span::styled(&visible, Style::default().fg(ACCENT)),
                ]),
            ])
            .block(panel(heading).title_bottom(" Enter 确认 · Esc 取消 · Ctrl+U 清空 "))
            .style(Style::default().bg(BG).fg(ACCENT)),
            dialog,
        );
        f.set_cursor_position((
            dialog.x + 4 + Line::from(visible.as_str()).width() as u16,
            dialog.y + 3,
        ));
    }
    if app.confirm_stop {
        let dialog = dialog_rect(area, 72, 7);
        f.render_widget(Clear, dialog);
        f.render_widget(
            Paragraph::new("游戏会立即终止，未保存的进度可能丢失。\nY 确认终止 · N / Esc 返回")
                .block(panel("终止游戏确认"))
                .style(Style::default().fg(Color::Yellow).bg(BG))
                .wrap(Wrap { trim: false }),
            dialog,
        );
    }
    if let Some(id) = &app.confirm_remove {
        let dialog = dialog_rect(area, 72, 7);
        f.render_widget(Clear, dialog);
        f.render_widget(
            Paragraph::new(format!(
                "将 {id} 移至回收站，数据可恢复。\nY 确认 · N / Esc 返回"
            ))
            .block(panel("移除实例"))
            .style(Style::default().bg(BG).fg(Color::Yellow))
            .wrap(Wrap { trim: false }),
            dialog,
        );
    }
    if app.help {
        let dialog = dialog_rect(area, 88, 16);
        f.render_widget(Clear, dialog);
        f.render_widget(
            Paragraph::new(format!(
                "{}\n\nTab / Shift+Tab 或 1–7 切换页面\n↑↓ / j k 选择 · Home / End 首尾\nPgUp / PgDn 每次移动 10 项\n任务：C 取消 · P 暂停 / 继续下载\nQ 空闲时退出",
                page_actions(app)
            ))
            .block(panel("当前页面 · 操作帮助").title_bottom(" ↑↓ 滚动 · F1 / Esc 返回 "))
            .style(Style::default().bg(BG).fg(TEXT))
            .scroll((app.help_scroll, 0))
            .wrap(Wrap { trim: false }),
            dialog,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn responsive_pages_and_dialogs_fit() {
        let mut app = app();
        for (width, height) in [(1, 1), (48, 16), (70, 20), (100, 30), (160, 45)] {
            let mut terminal =
                Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            for tab in 0..7 {
                app.tab = tab;
                terminal.draw(|f| draw(f, &app)).unwrap();
                app.help = true;
                terminal.draw(|f| draw(f, &app)).unwrap();
                app.help = false;
            }
            app.editor = Some(Editor {
                kind: EditKind::ImportPack,
                text: "长路径/".repeat(60) + "终点.mrpack",
            });
            let rendered = terminal.draw(|f| draw(f, &app)).unwrap();
            if width >= 48 {
                let text = rendered
                    .buffer
                    .content
                    .iter()
                    .map(|c| c.symbol())
                    .collect::<String>()
                    .replace(' ', "");
                assert!(text.contains("终点.mrpack"), "{width}x{height}: {text}");
                assert!(text.contains("7日志"));
            }
            app.editor = None;
            app.task = Some(WorkKind::Download);
            terminal.draw(|f| draw(f, &app)).unwrap();
            app.task = None;
            app.confirm_remove = Some("测试实例".into());
            terminal.draw(|f| draw(f, &app)).unwrap();
            app.confirm_remove = None;
        }
    }
    #[test]
    fn settings_navigation_and_unicode_tail() {
        let mut app = app();
        app.tab = 2;
        app.key(event::KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(app.setting, SETTINGS.len() - 1);
        app.key(event::KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(app.setting, SETTINGS.len() - 11);
        app.key(event::KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(app.setting, 0);
        assert_eq!(input_tail("abc中文xyz", 7), "中文xyz");
        assert_eq!(input_tail("中文", 1), "");
        app.tab = 6;
        app.key(event::KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(app.tab, 6);
        assert_eq!(app.log_horizontal, 8);
        app.key(event::KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(app.log_horizontal, 0);
    }
    #[test]
    fn loader_picker_scrolls_and_cancels_without_installing() {
        let mut app = app();
        app.loader_picker = Some(LoaderPicker {
            kind: crate::loader_catalog::InstallerKind::Fabric,
            game: "1.21.1".into(),
            selected: 0,
            versions: (0..100)
                .map(|i| crate::loader_catalog::LoaderVersion {
                    version: format!("0.1.{i}"),
                    stable: true,
                })
                .collect(),
        });
        assert!(!app.key(event::KeyEvent::new(KeyCode::End, KeyModifiers::NONE)));
        assert_eq!(app.loader_picker.as_ref().unwrap().selected, 100);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        let rendered = terminal.draw(|f| draw(f, &app)).unwrap();
        let text = rendered
            .buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("0.1.99"));
        assert!(!app.key(event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(app.loader_picker.is_none());
        assert!(app.task.is_none());
    }
    fn app() -> App {
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(Config::default(), tx, rx);
        app.local = vec!["1.21.1".into(), "fabric-loader-1.20.1".into()];
        app
    }
    #[test]
    fn all_pages_render() {
        let mut app = app();
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        for (tab, title) in [
            "本地游戏",
            "官方版本",
            "启动设置",
            "Mod管理",
            "账户管理",
            "实例回收站",
            "任务日志",
        ]
        .iter()
        .enumerate()
        {
            app.tab = tab;
            let rendered = terminal.draw(|f| draw(f, &app)).unwrap();
            // Inspect the intended frame: TestBackend keeps stale continuation
            // cells where real terminals overwrite both columns of a CJK glyph.
            let text = rendered
                .buffer
                .content
                .iter()
                .map(|c| c.symbol())
                .collect::<String>()
                .replace(' ', "");
            assert!(text.contains(title), "missing {title}: {text}");
        }
    }
    #[test]
    fn filtered_selection_matches_visible_item() {
        let mut app = app();
        app.query = "fabric".into();
        assert_eq!(app.selected().unwrap(), "fabric-loader-1.20.1");
        assert_eq!(app.count(), 1);
    }
    #[test]
    fn game_cancellation_requires_confirmation() {
        let mut app = app();
        app.task = Some(WorkKind::Game);
        app.key(event::KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        assert!(app.confirm_stop);
        assert!(!app.control.cancelled());
        app.key(event::KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(!app.control.cancelled());
        app.key(event::KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        app.key(event::KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(app.control.cancelled());
    }
    #[test]
    fn settings_list_scrolls_to_last_field() {
        let mut app = app();
        app.tab = 2;
        app.setting = 9;
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(70, 20)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>()
            .replace(' ', "");
        assert!(text.contains("窗口高度"));
    }
}

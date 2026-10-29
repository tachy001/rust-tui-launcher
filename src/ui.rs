use crate::localization::tr;
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
const PRIMARY: [(usize, &str); 5] = [
    (4, "账户"),
    (7, "实例管理"),
    (0, "实例列表"),
    (1, "下载"),
    (8, "设置"),
];
const INSTANCE_MENU: [&str; 13] = [
    "启动游戏",
    "游戏设置",
    "Mod 管理",
    "存档管理",
    "加载器 / 组件",
    "检查并修复",
    "安装建议 Java",
    "启动诊断",
    "游戏日志",
    "修改名称",
    "复制实例",
    "导出整合包",
    "移入回收站",
];
const SETTINGS_MENU: [&str; 12] = [
    "全局游戏设置",
    "Java 管理",
    "通用",
    "外观与界面语言",
    "下载",
    "高级与兼容性",
    "任务日志",
    "回收站",
    "导入整合包",
    "CurseForge API 密钥",
    "全部设置",
    "帮助",
];
const SETTING_GROUPS: [&[usize]; 6] = [
    &[19, 2, 4, 8, 9, 13, 14, 15, 17],
    &[1],
    &[0, 3, 10],
    &[16],
    &[7, 11, 12],
    &[20, 5, 6, 18],
];
const SETTINGS: [&str; 21] = [
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
    "微软应用 ID（高级覆盖）",
    "下载源（official / bmclapi / HTTPS）",
    "整合包可选文件（true / false）",
    "全屏启动（true / false）",
    "直连服务器（留空进入菜单）",
    "游戏语言（launcher 跟随 / zh_cn / en_us / keep 保持）",
    "界面语言 / UI language（zh_cn / en_us）",
    "快速进入存档（目录名，留空关闭）",
    "本地库兼容修补（true / false）",
    "自动分配内存（false 使用最大内存设置）",
    "最小内存 / MB（0 由 Java 决定）",
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
    task_popup: bool,
    task_handoff: bool,
    task_result: Option<(String, String)>,
    result_scroll: u16,
    result_max_scroll: std::cell::Cell<u16>,
    snapshots: bool,
    query: String,
    tx: Sender<Message>,
    rx: Receiver<Message>,
    editor: Option<Editor>,
    setting: usize,
    setting_group: Option<usize>,
    scope: Option<String>,
    mod_instance: Option<String>,
    mod_list: Vec<ModEntry>,
    confirm_stop: bool,
    language_picker: Option<usize>,
    log_scroll: usize,
    log_view_height: std::cell::Cell<usize>,
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
    managed_instance: Option<String>,
    page_history: Vec<(usize, usize)>,
    list_queries: [String; 2],
    nav_focused: bool,
}
impl App {
    fn new(config: Config, tx: Sender<Message>, rx: Receiver<Message>) -> Self {
        crate::localization::set(&config.language);
        let local = backend::installed(&config);
        let selection = config
            .last_version
            .as_ref()
            .and_then(|id| local.iter().position(|v| v == id))
            .unwrap_or(0);
        let mod_instance = config.last_version.clone();
        let names = crate::instances::names(&config);
        let managed_instance = local.get(selection).cloned();
        Self {
            config,
            tab: 0,
            selection,
            local,
            remote: vec![],
            logs: VecDeque::new(),
            task: None,
            control: TaskControl::default(),
            progress: (tr("空闲").into(), 0, 0),
            paused: false,
            task_popup: false,
            task_handoff: false,
            task_result: None,
            result_scroll: 0,
            result_max_scroll: std::cell::Cell::new(0),
            snapshots: false,
            query: String::new(),
            tx,
            rx,
            editor: None,
            setting: 0,
            setting_group: None,
            scope: None,
            mod_instance,
            mod_list: vec![],
            confirm_stop: false,
            language_picker: None,
            log_scroll: 0,
            log_view_height: std::cell::Cell::new(1),
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
            managed_instance,
            page_history: vec![],
            list_queries: Default::default(),
            nav_focused: true,
        }
    }
    fn log(&mut self, text: impl Into<String>) {
        for line in text.into().lines() {
            self.logs.push_back(line.to_owned());
            if self.logs.len() > 500 {
                self.logs.pop_front();
            }
            if self.log_scroll > 0 {
                self.log_scroll = self
                    .log_scroll
                    .saturating_add(1)
                    .min(self.logs.len().saturating_sub(self.log_view_height.get()));
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
            2 => self.setting_indices().len(),
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
            7 => INSTANCE_MENU.len(),
            8 => SETTINGS_MENU.len(),
            9 => 5,
            _ => 0,
        }
    }
    fn work(
        &mut self,
        kind: WorkKind,
        task: impl FnOnce(Backend) -> Result<String> + Send + 'static,
    ) {
        if self.task.is_some() {
            self.log(tr("已有任务正在运行"));
            return;
        }
        self.task = Some(kind);
        self.task_handoff = false;
        self.task_result = None;
        self.task_popup = kind != WorkKind::Game;
        self.pause_allowed = kind == WorkKind::Download;
        self.control = TaskControl::default();
        self.paused = false;
        self.progress = (tr("准备中").into(), 0, 0);
        self.transfer = None;
        let tx = self.tx.clone();
        let control = self.control.clone();
        let source = self.config.download_source.clone();
        let language = self.config.language.clone();
        thread::spawn(move || {
            crate::localization::set(&language);
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
            self.log(tr("本地版本已刷新"));
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
        if matches!(self.tab, 7..=9) {
            self.menu_action();
            return;
        }
        let config = self.config.clone();
        match self.tab {
            0 => {
                if let Some(id) = self.selected() {
                    if self.task.is_some() {
                        self.log(tr("请等待当前任务完成"));
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
                        Ok(tformat!("{} 安装完成", remote.id))
                    });
                }
            }
            2 => {
                if self.task.is_none() {
                    if self.scope.is_some() && global_only_setting(self.setting) {
                        self.log(tr("此项为全局设置，按 G 切换到全局后修改"));
                        return;
                    }
                    if matches!(self.setting, 4 | 12 | 13 | 18 | 19) {
                        let result =
                            parse_bool(&self.setting_value(self.setting)).and_then(|current| {
                                self.save_setting(if current { "false" } else { "true" })
                            });
                        if let Err(error) = result {
                            self.log(format!("{error:#}"));
                        }
                        return;
                    }
                    if self.setting == 16 {
                        if self.scope.is_some() {
                            self.log(tr("此项为全局设置，按 G 切换到全局后修改"));
                        } else {
                            let language =
                                crate::localization::UiLanguage::parse(&self.config.language).ok();
                            self.language_picker = Some(
                                if crate::localization::UiLanguage::is_system(&self.config.language)
                                {
                                    0
                                } else {
                                    crate::localization::UiLanguage::ALL
                                        .iter()
                                        .position(|value| Some(*value) == language)
                                        .map(|index| index + 1)
                                        .unwrap_or(0)
                                },
                            );
                        }
                    } else {
                        self.editor = Some(Editor {
                            kind: EditKind::Setting,
                            text: self.setting_value(self.setting),
                        });
                    }
                } else {
                    self.log(tr("请等待任务完成再修改设置"));
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
                    self.report(result.map(|restored| {
                        let mut message = tformat!("已恢复实例：{id}", id = restored.id);
                        if let Some(warning) = restored.cleanup_warning {
                            message.push('\n');
                            message.push_str(&warning);
                        }
                        message
                    }));
                    self.local = backend::installed(&self.config);
                    self.names = crate::instances::names(&self.config);
                    self.refresh_trash();
                }
            }
            _ => {}
        }
    }
    fn primary_index(&self) -> usize {
        match self.tab {
            4 => 0,
            7 | 9 | 3 => 1,
            2 if self.scope.is_some() => 1,
            6 if self.page_history.iter().any(|(page, _)| *page == 7) => 1,
            0 => 2,
            1 => 3,
            _ => 4,
        }
    }
    fn open_page(&mut self, page: usize) {
        if page == 2 {
            self.setting_group = None;
        }
        self.nav_focused = false;
        if self.tab < 2 {
            self.list_queries[self.tab] = self.query.clone();
        }
        self.page_history.push((self.tab, self.selection));
        self.tab = page;
        self.selection = 0;
        if page == 5 {
            self.refresh_trash();
        }
    }
    fn primary_page(&mut self, index: usize) {
        if self.tab < 2 {
            self.list_queries[self.tab] = self.query.clone();
        }
        if self.tab == 0 {
            if let Some(id) = self.selected() {
                self.managed_instance = Some(id);
            }
        }
        self.page_history.clear();
        self.tab = PRIMARY[index].0;
        if self.tab < 2 {
            self.query = self.list_queries[self.tab].clone();
        }
        self.selection = if self.tab == 0 {
            self.local_visible()
                .iter()
                .position(|id| Some(id.as_str()) == self.managed_instance.as_deref())
                .unwrap_or(0)
        } else {
            0
        };
    }
    fn back_page(&mut self) {
        if let Some((page, selection)) = self.page_history.pop() {
            self.tab = page;
            if page < 2 {
                self.query = self.list_queries[page].clone();
            }
            self.selection = selection.min(self.count().saturating_sub(1));
        } else if self.tab == 7 {
            self.primary_page(2);
        } else if !PRIMARY.iter().any(|(page, _)| *page == self.tab) {
            self.tab = if matches!(self.tab, 3 | 9) || (self.tab == 2 && self.scope.is_some()) {
                7
            } else {
                8
            };
            self.selection = 0;
        }
    }
    fn menu_action(&mut self) {
        let parent = (self.tab, self.selection);
        if self.task.is_some() && !(self.tab == 8 && matches!(self.selection, 6 | 7 | 11)) {
            self.task_popup = true;
            return;
        }
        if self.tab == 8 {
            match self.selection {
                10 => {
                    self.scope = None;
                    self.open_page(2);
                }
                7 => self.open_page(5),
                6 => self.open_page(6),
                11 => self.help = true,
                8 => {
                    self.editor = Some(Editor {
                        kind: EditKind::ImportPack,
                        text: String::new(),
                    })
                }
                9 => {
                    self.editor = Some(Editor {
                        kind: EditKind::CurseKey,
                        text: String::new(),
                    })
                }
                0..=5 => {
                    let group = self.selection;
                    self.scope = None;
                    self.open_page(2);
                    self.setting_group = Some(group);
                    self.setting = SETTING_GROUPS[group][0];
                }
                _ => {}
            }
            return;
        }
        let Some(id) = self
            .managed_instance
            .clone()
            .filter(|id| self.local.contains(id))
        else {
            self.log(tr("请先在实例列表选择一个已安装实例"));
            self.primary_page(2);
            return;
        };
        if self.tab == 7 && self.selection == 4 {
            self.open_page(9);
            return;
        }
        let code = if self.tab == 9 {
            [
                KeyCode::Char('f'),
                KeyCode::Char('u'),
                KeyCode::Char('h'),
                KeyCode::Char('n'),
                KeyCode::F(7),
            ][self.selection.min(4)]
        } else {
            [
                KeyCode::Enter,
                KeyCode::Char('o'),
                KeyCode::Char('m'),
                KeyCode::Char('w'),
                KeyCode::Null,
                KeyCode::Char('x'),
                KeyCode::Char('b'),
                KeyCode::Char('d'),
                KeyCode::Char('l'),
                KeyCode::F(2),
                KeyCode::Char('t'),
                KeyCode::Char('e'),
                KeyCode::Delete,
            ][self.selection.min(12)]
        };
        let query = std::mem::take(&mut self.query);
        self.tab = 0;
        self.selection = self
            .local
            .iter()
            .position(|entry| entry == &id)
            .unwrap_or(0);
        if code == KeyCode::Enter {
            self.action();
        } else {
            self.key(event::KeyEvent::new(code, KeyModifiers::NONE));
        }
        self.query = query;
        if self.tab == 0 {
            (self.tab, self.selection) = parent;
        } else {
            self.page_history.push(parent);
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
                Ok(tformat!("找到 {total} 个兼容项目", total = total))
            });
        } else {
            self.log(tr("请先从实例列表进入实例管理 › Mod 管理"));
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
                    Ok(tformat!(
                        "{count} 个兼容版本，Enter 安装所选版本",
                        count = count
                    ))
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
                Ok(tformat!(
                    "发现 {total} 个在线管理项目可更新，Enter 安装",
                    total = total
                ))
            });
        }
    }
    fn report(&mut self, result: Result<String>) {
        match result {
            Ok(s) => self.log(s),
            Err(e) => self.log(tformat!("错误 · {e:#}", e = e)),
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
                    Ok(tformat!("{id} 已校验修复", id = id))
                });
            }
        }
    }
    fn setting_indices(&self) -> Vec<usize> {
        self.setting_group
            .map(|group| SETTING_GROUPS[group].to_vec())
            .unwrap_or_else(|| (0..SETTINGS.len()).collect())
    }
    fn setting_position(&self) -> usize {
        self.setting_indices()
            .iter()
            .position(|index| *index == self.setting)
            .unwrap_or(0)
    }
    fn setting_value(&self, index: usize) -> String {
        let c = match self
            .scope
            .as_ref()
            .map(|id| self.config.effective(id))
            .unwrap_or_else(|| Ok(self.config.clone()))
        {
            Ok(config) => config,
            Err(error) => return tformat!("配置读取失败：{error:#}", error = error),
        };
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
            14 => c.server,
            15 => c.game_language,
            16 => c.language,
            17 => c.quick_play_world,
            18 => c.patch_natives.to_string(),
            19 => c.auto_memory.to_string(),
            20 => c.min_memory_mb.to_string(),
            _ => String::new(),
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
                8 => instance.width = Some(value.parse().context("宽度必须为整数")?),
                9 => instance.height = Some(value.parse().context("高度必须为整数")?),
                13 => instance.fullscreen = Some(parse_bool(value)?),
                14 => instance.server = Some(value.trim().into()),
                15 => instance.game_language = Some(value.trim().into()),
                17 => instance.quick_play_world = Some(value.trim().into()),
                18 => instance.patch_natives = Some(parse_bool(value)?),
                19 => instance.auto_memory = Some(parse_bool(value)?),
                20 => instance.min_memory_mb = Some(value.parse().context("内存必须为整数 MB")?),
                _ => bail!("此项为全局设置，按 G 切换到全局后修改"),
            }
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
                14 => candidate.server = value.trim().into(),
                15 => candidate.game_language = value.trim().into(),
                16 => candidate.language = value.trim().into(),
                17 => candidate.quick_play_world = value.trim().into(),
                18 => candidate.patch_natives = parse_bool(value)?,
                19 => candidate.auto_memory = parse_bool(value)?,
                20 => candidate.min_memory_mb = value.parse().context("内存必须为整数 MB")?,
                _ => bail!("无效设置项"),
            }
        }
        if let Some(id) = &self.scope {
            crate::instances::save_settings(&candidate, id)?;
        } else {
            candidate.save()?;
        }
        self.config = candidate;
        crate::localization::set(&self.config.language);
        self.local = backend::installed(&self.config);
        self.log(tr("设置已保存"));
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
                            Ok(tformat!("存档已备份：{}", output.display()))
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
                        Ok(tformat!("存档已导入：{name}", name = name))
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
                        Ok(tformat!("实例已复制：{target}", target = target))
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
                        Ok(tformat!("导出完成：{}", path.display()))
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
                    Ok(tformat!("整合包已导入：{id}", id = id))
                });
                Ok(())
            }
            EditKind::ImportMod => self
                .mod_instance
                .as_ref()
                .context("请先从实例列表进入实例管理 › Mod 管理")
                .and_then(|id| {
                    mods::import(&self.config, id, Path::new(editor.text.trim_matches('"')))
                }),
        };
        if let Err(e) = result {
            self.log(e.to_string());
            self.editor = Some(editor);
        } else if matches!(editor.kind, EditKind::CurseKey) {
            self.log(tr("CurseForge API 密钥已保存在系统安全存储"));
        } else if matches!(editor.kind, EditKind::NameInstance) {
            self.names = crate::instances::names(&self.config);
            self.log(tr("显示名称已更新，实例 ID 与依赖关系保持稳定"));
        } else if matches!(editor.kind, EditKind::ImportMod) {
            self.log(tr("Mod 已导入"));
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
                    self.task_handoff = true;
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
                    let show_result = (self.task_popup && !self.task_handoff) || result.is_err();
                    if show_result {
                        let title = if self.control.cancelled() && result.is_err() {
                            tr("任务已取消")
                        } else if result.is_ok() {
                            tr("任务已完成")
                        } else {
                            tr("任务失败")
                        };
                        let message = match &result {
                            Ok(message) | Err(message) => message.clone(),
                        };
                        self.task_result = Some((title.into(), message));
                        self.result_scroll = 0;
                    }
                    self.task = None;
                    self.task_popup = false;
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
                        Err(e) => self.log(tformat!("错误 · {e}", e = e)),
                    }
                }
            }
        }
        if self.tab != 2 {
            self.selection = self.selection.min(self.count().saturating_sub(1));
        }
    }
    fn key(&mut self, key: event::KeyEvent) -> bool {
        let repeatable = matches!(
            key.code,
            KeyCode::Up
                | KeyCode::Down
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::PageUp
                | KeyCode::PageDown
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::Backspace
        );
        if key.kind == KeyEventKind::Release || (key.kind == KeyEventKind::Repeat && !repeatable) {
            return false;
        }
        if let Some(selected) = self.language_picker {
            let count = crate::localization::UiLanguage::ALL.len() + 1;
            match key.code {
                KeyCode::Up => self.language_picker = Some((selected + count - 1) % count),
                KeyCode::Down => self.language_picker = Some((selected + 1) % count),
                KeyCode::Home => self.language_picker = Some(0),
                KeyCode::End => self.language_picker = Some(count - 1),
                KeyCode::Esc => self.language_picker = None,
                KeyCode::Enter => {
                    let code = if selected == 0 {
                        "auto"
                    } else {
                        crate::localization::UiLanguage::ALL[selected - 1].game_code()
                    };
                    match self.save_setting(code) {
                        Ok(()) => self.language_picker = None,
                        Err(error) => self.log(format!("{error:#}")),
                    }
                }
                _ => {}
            }
            return false;
        }
        if self.task_result.is_some()
            && !self.help
            && self.editor.is_none()
            && !self.confirm_stop
            && self.confirm_remove.is_none()
        {
            match key.code {
                KeyCode::Up => self.result_scroll = self.result_scroll.saturating_sub(1),
                KeyCode::Down => {
                    self.result_scroll = self
                        .result_scroll
                        .saturating_add(1)
                        .min(self.result_max_scroll.get())
                }
                KeyCode::PageUp => self.result_scroll = self.result_scroll.saturating_sub(8),
                KeyCode::PageDown => {
                    self.result_scroll = self
                        .result_scroll
                        .saturating_add(8)
                        .min(self.result_max_scroll.get())
                }
                KeyCode::Home => self.result_scroll = 0,
                KeyCode::End => self.result_scroll = self.result_max_scroll.get(),
                KeyCode::Enter | KeyCode::Esc => self.task_result = None,
                KeyCode::Char('l') => {
                    self.task_result = None;
                    self.world_picker = None;
                    self.loader_picker = None;
                    self.tab = 6;
                }
                _ => {}
            }
            return false;
        }
        if self.task_popup
            && self.task.is_some()
            && !self.confirm_stop
            && !self.help
            && self.editor.is_none()
            && self.confirm_remove.is_none()
        {
            match key.code {
                KeyCode::Esc | KeyCode::F(6) => self.task_popup = false,
                KeyCode::Char('l') => {
                    self.task_popup = false;
                    self.world_picker = None;
                    self.loader_picker = None;
                    self.tab = 6;
                }
                KeyCode::Char('p') if self.pause_allowed && !self.control.cancelled() => {
                    self.paused = self.control.toggle_pause();
                }
                KeyCode::Char('c') if self.task == Some(WorkKind::Game) => {
                    self.confirm_stop = true;
                }
                KeyCode::Char('c') => {
                    self.control.cancel();
                    self.paused = false;
                }
                KeyCode::F(1) => self.help = true,
                _ => {}
            }
            return false;
        }
        if let Some(id) = self.confirm_remove.clone() {
            match key.code {
                KeyCode::Char('y') => {
                    let result = crate::instances::trash(&self.config, &id);
                    self.report(result.map(|entry| {
                        tformat!("实例已移至回收站，可在设置 › 回收站恢复：{}", entry.label)
                    }));
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
                    self.log(tr("正在终止游戏…"));
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
        if key.code == KeyCode::F(6) && self.task.is_some() {
            self.task_popup = true;
            return false;
        }
        if self.task.is_some() && key.code == KeyCode::Char('c') {
            if self.task == Some(WorkKind::Game) {
                self.confirm_stop = true;
            } else {
                self.control.cancel();
                self.paused = false;
                self.log(tr("正在取消，等待当前操作结束…"));
            }
            return false;
        }
        if self.task.is_some() && key.code == KeyCode::Char('p') && self.pause_allowed {
            if !self.control.cancelled() {
                self.paused = self.control.toggle_pause();
            }
            return false;
        }
        if let Some(picker) = &mut self.world_picker {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => self.world_picker = None,
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
                        Err(error) => self.log(tformat!("读取存档失败：{error}", error = error)),
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
                        Ok(tformat!(
                            "安装完成：{instance}，请前往本地版本启动",
                            instance = instance
                        ))
                    });
                }
                _ => (),
            }
            return false;
        }
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            self.nav_focused = !self.nav_focused;
            return false;
        }
        if self.nav_focused {
            match key.code {
                KeyCode::Down | KeyCode::Char('j') => {
                    self.primary_page((self.primary_index() + 1) % PRIMARY.len());
                    return false;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.primary_page((self.primary_index() + PRIMARY.len() - 1) % PRIMARY.len());
                    return false;
                }
                KeyCode::Home => {
                    self.primary_page(0);
                    return false;
                }
                KeyCode::End => {
                    self.primary_page(PRIMARY.len() - 1);
                    return false;
                }
                KeyCode::Enter | KeyCode::Right => {
                    self.nav_focused = false;
                    return false;
                }
                KeyCode::Esc | KeyCode::Left => return false,
                _ => {}
            }
        } else if key.code == KeyCode::Left && !key.modifiers.contains(KeyModifiers::ALT) {
            self.nav_focused = true;
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
                        Err(error) => self.log(tformat!("读取存档失败：{error}", error = error)),
                    }
                }
            }
            KeyCode::F(6) if self.task.is_some() => self.task_popup = true,
            KeyCode::F(1) => self.help = true,
            KeyCode::Esc => {
                if self.page_history.is_empty() && PRIMARY.iter().any(|(page, _)| *page == self.tab)
                {
                    self.nav_focused = true;
                } else {
                    self.back_page();
                }
            }
            KeyCode::Char('q') => {
                if self.task.is_none() {
                    return true;
                } else {
                    self.log(tr("任务正在运行；C 取消任务，游戏终止需再按 Y 确认"));
                }
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.task.is_none() {
                    return true;
                } else {
                    self.log(tr("请按 C 取消任务后退出"));
                }
            }
            KeyCode::Right if self.tab == 6 && key.modifiers.contains(KeyModifiers::ALT) => {
                self.log_horizontal = self.log_horizontal.saturating_add(8);
            }
            KeyCode::Left if self.tab == 6 && key.modifiers.contains(KeyModifiers::ALT) => {
                self.log_horizontal = self.log_horizontal.saturating_sub(8);
            }
            KeyCode::Char(c @ '1'..='5') => {
                self.primary_page(c as usize - '1' as usize);
                self.nav_focused = false;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.tab == 6 {
                    self.log_scroll = self.log_scroll.saturating_sub(1);
                } else if self.tab == 2 {
                    let indices = self.setting_indices();
                    self.setting = indices[(self.setting_position() + 1) % indices.len()];
                } else if self.count() > 0 {
                    self.selection = (self.selection + 1) % self.count();
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.tab == 6 {
                    self.log_scroll = self
                        .log_scroll
                        .saturating_add(1)
                        .min(self.logs.len().saturating_sub(self.log_view_height.get()));
                } else if self.tab == 2 {
                    let indices = self.setting_indices();
                    self.setting =
                        indices[(self.setting_position() + indices.len() - 1) % indices.len()];
                } else if self.count() > 0 {
                    self.selection = (self.selection + self.count() - 1) % self.count();
                }
            }
            KeyCode::PageUp if self.tab == 6 => {
                self.log_scroll = self
                    .log_scroll
                    .saturating_add(self.log_view_height.get())
                    .min(self.logs.len().saturating_sub(self.log_view_height.get()))
            }
            KeyCode::PageDown if self.tab == 6 => {
                self.log_scroll = self.log_scroll.saturating_sub(self.log_view_height.get())
            }
            KeyCode::Home if self.tab == 6 => {
                self.log_scroll = self.logs.len().saturating_sub(self.log_view_height.get())
            }
            KeyCode::End if self.tab == 6 => self.log_scroll = 0,
            KeyCode::Home | KeyCode::End | KeyCode::PageUp | KeyCode::PageDown => {
                let last = self.count().saturating_sub(1);
                if self.tab == 2 {
                    let position = self.setting_position();
                    let next = match key.code {
                        KeyCode::Home => 0,
                        KeyCode::End => last,
                        KeyCode::PageUp => position.saturating_sub(10),
                        _ => position.saturating_add(10).min(last),
                    };
                    self.setting = self.setting_indices()[next];
                    return false;
                }
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
            KeyCode::Enter if self.tab == 0 => {
                if let Some(id) = self.selected() {
                    self.managed_instance = Some(id);
                    self.open_page(7);
                }
            }
            KeyCode::Char(' ') if self.tab == 0 => self.action(),
            KeyCode::Char(' ') if self.tab == 7 => {
                let selection = self.selection;
                self.selection = 0;
                self.menu_action();
                self.selection = selection;
            }
            KeyCode::Backspace if self.tab != 3 => self.back_page(),
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
                    self.report(
                        result.map(|filename| tformat!("已恢复 {filename}", filename = filename)),
                    );
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
                        .map(|backup| tformat!("Mod 已移出，备份：{}", backup.display()));
                    self.report(result);
                    self.refresh_mods();
                }
            }
            KeyCode::Char('l') if self.tab == 0 => {
                if let Some(id) = self.selected() {
                    let result = self
                        .config
                        .effective(&id)
                        .and_then(|config| config.instance_dir(&id))
                        .and_then(|dir| crate::logs::recent(&dir.join("tui-mc-game.log")));
                    match result {
                        Ok(lines) => {
                            for line in lines {
                                self.log(tformat!("游戏 · {line}", line = line));
                            }
                            self.tab = 6;
                            self.log_scroll = 0;
                        }
                        Err(e) => self.log(tformat!("读取日志失败：{e}", e = e)),
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
                        Err(error) => self.log(tformat!("诊断失败：{error}", error = error)),
                    }
                }
            }
            KeyCode::Char('b') if self.tab == 0 => {
                if let Some(id) = self.selected() {
                    let config = self.config.clone();
                    self.work(WorkKind::Download, move |b| {
                        let runtime = b.install_java_for_version(&config, &id)?;
                        Ok(tformat!(
                            "Java {} 已就绪：{}",
                            runtime.major(),
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
                    self.selection = 0;
                } else if self.mod_search.is_some() {
                    self.mod_search = None;
                    self.selection = 0;
                } else {
                    self.back_page();
                }
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
            KeyCode::F(7) if self.tab < 2 => {
                self.choose_loader(crate::loader_catalog::InstallerKind::OptiFine)
            }
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
                    self.setting_group = None;
                    self.setting = 1;
                }
            }
            KeyCode::Char('g') if self.tab == 2 => {
                self.scope = None;
                self.log(tr("已切换为全局设置"));
            }
            KeyCode::Char('d') if self.tab == 2 && self.task.is_none() => {
                if let Some(id) = &self.scope {
                    let result = crate::instances::reset_settings(&mut self.config, id);
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
                    let found = java::discover(&config, &b.control)?;
                    for runtime in &found {
                        b.log(format!(
                            "Java {} · {}",
                            runtime.major(),
                            runtime.path.display()
                        ));
                    }
                    if found.is_empty() {
                        bail!("没有发现可用 Java");
                    }
                    Ok(tformat!(
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
                        self.log(tr("正在取消，等待当前网络读取返回…"));
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
fn global_only_setting(index: usize) -> bool {
    matches!(index, 0 | 3 | 7 | 10 | 11 | 12 | 16)
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
    crate::localization::set(&config.language);
    enable_raw_mode()?;
    let _guard = TerminalGuard;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let (tx, rx) = mpsc::channel();
    let mut app = App::new(config, tx, rx);
    app.log(tr(
        "欢迎使用 Tui-Launcher。Enter 管理所选实例 · Space 启动 · 1–5 一级导航 · Esc 返回上一级",
    ));
    app.log(tr(
        "账户 › 微软登录：按 A 打开浏览器授权，完成后自动返回启动器。",
    ));
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
            Paragraph::new(tr("\n  暂无内容\n\n  F1 查看当前页操作"))
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
        0 => tr("Enter 管理所选实例 · Space 启动\n/ 搜索 · R 刷新\n组件、Mod、存档与修复请进入实例管理"),
        1 => tr("Enter 安装原版 · R 获取版本目录\nS 切换快照 · / 搜索\n安装后的加载器与 Mod 请在实例管理中配置"),
        7 => tr("↑↓ 选择功能 · Enter 打开\nEsc 返回实例列表 · Space 启动游戏"),
        8 => tr("↑↓ 选择设置分类 · Enter 打开\nEsc 返回上一级"),
        9 => tr("↑↓ 选择加载器 · Enter 选择具体版本\nEsc 返回实例管理"),
        6 => tr("↑↓ 滚动 · PgUp / PgDn 翻页\n← → 切页 · Alt+← → 查看长行\nHome 最早记录 · End 跟随最新记录"),
        2 => tr("Enter 编辑 · G 全局设置\nD 清除当前实例覆盖 · V 检测 Java\n编辑：Enter 保存 · Esc 取消 · Ctrl+U 清空"),
        3 if app.mod_versions.is_some() => tr("Enter 安装所选版本\nBackspace 返回搜索结果 · L 本地 Mod"),
        3 if app.mod_search.is_some() => tr("Enter 安装 · V 选择版本 · B 搜索\nN 下一页 · P 上一页 · L 本地 Mod"),
        3 => tr("Enter 启停 · A 导入 JAR · B 在线搜索\nU 检查更新 · Del 移出 · Z 恢复 · R 刷新\nEsc 返回实例管理"),
        4 => tr("Enter 切换账户 · N 添加离线账户\nA 微软登录 · D 移除账户\nShift+K 配置 CurseForge 密钥\n微软登录：浏览器授权后自动返回"),
        _ => tr("Enter 恢复实例 · R 刷新\nEsc 返回设置菜单"),
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
    crate::localization::set(&app.config.language);
    let area = f.area();
    f.render_widget(
        Block::default().style(Style::default().bg(BG).fg(TEXT)),
        area,
    );
    if area.width < 48 || area.height < 16 {
        f.render_widget(
            Paragraph::new(tr("请将终端调整至至少 48 × 16\nQ 空闲时退出"))
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
    let mut rows = [rows[0], rows[1], rows[2], rows[3], rows[4]];
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
            panel(tr("方块之间 · 即刻出发"))
        } else {
            Block::default()
        }),
        rows[0],
    );
    if area.width >= 80 {
        let panes = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(22), Constraint::Min(30)])
            .split(rows[2]);
        rows[2] = panes[1];
        let items: Vec<ListItem> = PRIMARY
            .iter()
            .enumerate()
            .map(|(index, (_, name))| ListItem::new(format!(" {}  {}", index + 1, tr(name))))
            .collect();
        let mut nav_state = ListState::default().with_selected(Some(app.primary_index()));
        f.render_stateful_widget(
            List::new(items)
                .block(
                    panel(if app.nav_focused {
                        tr("导航 · ↑↓ 选择")
                    } else {
                        tr("导航 · Tab 返回")
                    })
                    .border_style(Style::default().fg(if app.nav_focused {
                        ACCENT
                    } else {
                        MUTED
                    })),
                )
                .highlight_style(if app.nav_focused {
                    highlight()
                } else {
                    Style::default().fg(ACCENT)
                })
                .highlight_symbol(if app.nav_focused { "▸ " } else { "  " }),
            panes[0],
            &mut nav_state,
        );
        let section = match app.tab {
            2 => {
                if app.scope.is_some() {
                    tr("游戏设置")
                } else {
                    tr("全局游戏与下载设置")
                }
            }
            3 => tr("Mod 管理"),
            5 => tr("回收站"),
            6 => tr("日志"),
            9 => tr("加载器 / 组件"),
            _ => PRIMARY[app.primary_index()].1,
        };
        let context = if app.primary_index() == 1 {
            app.managed_instance.as_deref().unwrap_or(tr("未选择实例"))
        } else {
            ""
        };
        f.render_widget(
            Paragraph::new(format!(
                " {}  ›  {section}  {context}",
                PRIMARY[app.primary_index()].1
            ))
            .style(Style::default().fg(MUTED)),
            rows[1],
        );
    } else {
        let tabs = [
            tr("1账户"),
            tr("2实例"),
            tr("3列表"),
            tr("4下载"),
            tr("5设置"),
        ];
        f.render_widget(
            Paragraph::new(Line::from(
                tabs.iter()
                    .enumerate()
                    .map(|(i, t)| {
                        Span::styled(
                            format!("{t} "),
                            Style::default()
                                .fg(if app.primary_index() == i { BG } else { MUTED })
                                .bg(if app.primary_index() == i { ACCENT } else { BG }),
                        )
                    })
                    .collect::<Vec<_>>(),
            )),
            rows[1],
        );
    }
    let cols = if rows[2].width >= 100 {
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
        7..=9 => {
            let (title, labels): (String, Vec<&str>) = match app.tab {
                7 => (
                    tformat!(
                        "实例管理 · {}",
                        app.managed_instance
                            .as_ref()
                            .map(|id| app.names.get(id).unwrap_or(id).as_str())
                            .unwrap_or(tr("请先选择实例"))
                    ),
                    INSTANCE_MENU.to_vec(),
                ),
                8 => (tr("设置").into(), SETTINGS_MENU.to_vec()),
                _ => (
                    tr("实例管理 › 加载器 / 组件").into(),
                    vec!["Fabric", "Quilt", "Forge", "NeoForge", "OptiFine"],
                ),
            };
            let items = labels
                .into_iter()
                .map(|label| ListItem::new(format!(" › {}", tr(label))))
                .collect();
            render_list(f, items, title, app.selection, rows[2]);
        }
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
                    tr("实例列表")
                } else {
                    tr("官方版本")
                },
                items.len(),
                if app.query.is_empty() {
                    String::new()
                } else {
                    format!(" · /{}", app.query)
                }
            );
            if items.is_empty() {
                let empty = if !app.query.is_empty() {
                    tr("\n 没有匹配的版本。\n 按 /，Ctrl+U 清空搜索后 Enter。")
                } else if app.tab == 0 {
                    tr("\n 还没有游戏实例。\n 按 4 前往下载，R 获取官方列表。\n 已有游戏可在设置中指定游戏目录。")
                } else {
                    tr("\n R 获取 Mojang 官方版本目录。\n Enter 安装原版 · S 显示快照 · / 搜索\n 加载器和 Mod 在安装后的实例管理中配置。")
                };
                f.render_widget(
                    Paragraph::new(empty)
                        .block(panel(title))
                        .wrap(Wrap { trim: false }),
                    cols[0],
                );
            } else {
                render_list(f, items, title, app.selection, cols[0]);
            }
            let selected = app.selected().unwrap_or_else(|| tr("尚未选择").into());
            let effective = app.config.effective(&selected);
            let configuration = match &effective {
                Ok(config) => format!(
                    "{} MB · {}",
                    config.memory_mb,
                    if config.isolated {
                        tr("独立实例目录")
                    } else {
                        tr("共享游戏目录")
                    }
                ),
                Err(error) => tformat!("配置读取失败：{error:#}", error = error),
            };
            let help = if app.tab == 0 {
                tr("Enter 进入实例管理\nSpace 启动游戏\n\n游戏设置、Mod、存档、组件\n均在所选实例的管理菜单中")
            } else {
                tr("Enter 安装原版\nS 快照 · / 搜索\n\n安装后进入实例管理\n选择组件与 Mod")
            };
            f.render_widget(
                Paragraph::new(format!(
                    "{selected}\n\n{}\n{configuration}\n\n{help}",
                    app.accounts
                        .active()
                        .map(|a| format!(
                            "{} · {}",
                            a.name,
                            if a.kind == AccountKind::Microsoft {
                                tr("微软")
                            } else {
                                tr("离线")
                            }
                        ))
                        .unwrap_or_else(|| tformat!("{} · 离线", app.config.username)),
                ))
                .block(panel(tr("当前选择")))
                .wrap(Wrap { trim: false }),
                cols[1],
            );
        }
        6 => {
            let height = rows[2].height.saturating_sub(2) as usize;
            app.log_view_height.set(height.max(1));
            let scroll = app
                .log_scroll
                .min(app.logs.len().saturating_sub(height.max(1)));
            let end = app.logs.len().saturating_sub(scroll);
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
                    panel(tr("任务日志")).title_bottom(format!(
                        " {}–{} / {} · {} ",
                        if end == 0 { 0 } else { start + 1 },
                        end,
                        app.logs.len(),
                        if app.log_scroll == 0 {
                            tr("跟随最新")
                        } else {
                            tr("历史记录")
                        }
                    )),
                ),
                rows[2],
            );
        }
        2 => {
            let items = app
                .setting_indices()
                .into_iter()
                .map(|i| {
                    let label = SETTINGS[i];
                    let global_only = app.scope.is_some() && global_only_setting(i);
                    ListItem::new(vec![
                        Line::from(Span::styled(
                            if global_only {
                                format!("{} · {}", tr(label), tr("仅全局"))
                            } else {
                                tr(label).to_owned()
                            },
                            Style::default().fg(MUTED),
                        )),
                        Line::from(if !global_only && matches!(i, 4 | 12 | 13 | 18 | 19) {
                            format!("  {}  · {}", app.setting_value(i), tr("Enter 切换"))
                        } else {
                            format!("  {}", app.setting_value(i))
                        }),
                    ])
                })
                .collect();
            let scope = app
                .scope
                .as_ref()
                .map(|id| tformat!("实例：{id}", id = id))
                .unwrap_or_else(|| tr("全局").into());
            render_list(
                f,
                items,
                app.setting_group
                    .map(|group| format!("{} · {scope}", tr(SETTINGS_MENU[group])))
                    .unwrap_or_else(|| tformat!("启动设置 · {scope}", scope = scope)),
                app.setting_position(),
                cols[0],
            );
            f.render_widget(Paragraph::new(tformat!("\n Enter 编辑 / 保存\n Ctrl+U 清空输入\n Esc 取消编辑\n\n G 全局设置\n D 清除实例覆盖\n V 检测 Java\n\n 实例可覆盖 Java、内存、\n 隔离与附加参数。\n\n {}",app.logs.back().map(String::as_str).unwrap_or(""))).block(panel(tr("说明"))).wrap(Wrap{trim:false}),cols[1]);
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
            render_list(
                f,
                items,
                tr("选择兼容 Mod 版本").into(),
                app.selection,
                cols[0],
            );
            let selected = versions
                .get(app.selection)
                .map(|v| v.name.as_str())
                .unwrap_or(tr("没有兼容版本"));
            f.render_widget(Paragraph::new(tformat!("\n {selected}\n {project}\n\n Enter 安装所选版本\n Backspace 返回项目\n L 本地 Mod 列表\n\n 自动处理必需依赖，\n 安装前保留旧文件备份。\n\n {}",app.logs.back().map(String::as_str).unwrap_or(""), project = project, selected = selected)).block(panel(tr("版本安装"))).wrap(Wrap{trim:false}),cols[1]);
        }
        3 if app.mod_search.is_some() => {
            let result = app.mod_search.as_ref().unwrap();
            let items = result
                .hits
                .iter()
                .map(|hit| ListItem::new(format!(" {} · {}", hit.title, hit.slug)))
                .collect();
            let title = tformat!(
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
                .unwrap_or(tr("没有匹配项目，按 B 修改关键词"));
            f.render_widget(Paragraph::new(tformat!("\n {description}\n\n Enter 安装兼容稳定版\n V 选择具体版本\n B / / 搜索\n N / P 下一页 / 上一页\n U 检查已管理 Mod 更新\n L 返回本地 Mod\n\n {}",app.logs.back().map(String::as_str).unwrap_or(""), description = description)).block(panel("Modrinth")).wrap(Wrap{trim:false}),cols[1]);
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
                tformat!(
                    "Mod 管理 · {}",
                    app.mod_instance.as_deref().unwrap_or(tr("请先选择实例"))
                ),
                app.selection,
                cols[0],
            );
            f.render_widget(Paragraph::new(tr("\n Enter 启用 / 禁用\n Del 移出并备份\n Z 恢复最近移除项\n A 导入本地 JAR\n B / / 在线搜索\n U 检查更新 · L 本地列表\n R 刷新\n\n 在本地版本页按 M 选择实例。\n\n 禁用使用 .jar.disabled 后缀，\n 原文件内容保持完整。\n\n Mod 变更在下次启动时生效。")).block(panel(tr("管理"))).wrap(Wrap{trim:false}),cols[1]);
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
                            tr("微软正版")
                        } else {
                            tr("离线")
                        }
                    ))
                })
                .collect();
            render_list(f, items, tr("账户管理").into(), app.selection, cols[0]);
            f.render_widget(Paragraph::new(tformat!("\n Enter 选择账户\n N 添加离线账户\n A 浏览器登录微软账户\n Shift+K 配置 CurseForge 密钥\n D 移除账户与本机凭证\n\n 浏览器授权后自动返回启动器。\n 微软凭证保存在系统安全存储。\n 无法使用浏览器时可运行\n account login-device。\n\n {}",app.logs.back().map(String::as_str).unwrap_or(""))).block(panel(tr("登录"))).wrap(Wrap{trim:false}),cols[1]);
        }
        _ => {
            let items = app
                .trash
                .iter()
                .map(|entry| ListItem::new(format!(" {} · {}", entry.label, entry.id)))
                .collect();
            render_list(f, items, tr("实例回收站").into(), app.selection, cols[0]);
            f.render_widget(Paragraph::new(tformat!("\n Enter 恢复选择的实例\n R 刷新\n\n 同名实例存在时不会覆盖。\n 回收站保留实例目录的数据。\n\n 在本地页按 Delete 移入。\n 有其他实例依赖的父版本\n 不能直接移除。\n\n {}",app.logs.back().map(String::as_str).unwrap_or(""))).block(panel(tr("恢复"))).wrap(Wrap{trim:false}),cols[1]);
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
        tr("已暂停 · P 继续 / C 取消")
    } else if app.task.is_some() && app.pause_allowed {
        tr("进行中 · P 暂停下载 / C 取消")
    } else if app.task.is_some() {
        tr("进行中 · C 取消")
    } else {
        tr("就绪")
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
                            tformat!("{} · 等待数据或校验中", transfer.label)
                        } else {
                            transfer.display(app.paused)
                        }
                    } else {
                        format!("{label} {done}/{total}")
                    }
                } else {
                    tr("等待下一次冒险").into()
                }),
            rows[3],
        );
    } else {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(tr(" ● 就绪  "), Style::default().fg(ACCENT)),
                Span::raw(
                    app.logs
                        .back()
                        .map(String::as_str)
                        .unwrap_or(tr("选择游戏，开始冒险")),
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
                if app.nav_focused {
                    tr("↑↓ 选择导航 · Enter / → 进入 · Tab 切换焦点")
                } else {
                    tr("↑↓ 选择内容 · ← / Tab 导航 · Esc 返回 · F6 任务")
                },
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
            vec![ListItem::new(tr("暂无存档，按 I 导入 ZIP"))]
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
                    panel(tformat!("{} · 存档", picker.instance))
                        .title_bottom(tr(" Enter 备份 · I 导入 · R 刷新 · Esc 返回 ")),
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
        let mut items = vec![ListItem::new(tr("自动选择推荐 / 稳定版本"))];
        items.extend(picker.versions.iter().map(|entry| {
            ListItem::new(format!(
                "{}{}",
                entry.version,
                if entry.stable { "" } else { tr("  [预览]") }
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
                    .title_bottom(tr(" ↑↓ / PgUp / PgDn 选择 · Enter 安装 · Esc 返回 ")),
                )
                .style(Style::default().bg(BG).fg(TEXT))
                .highlight_style(Style::default().bg(ACCENT).fg(BG)),
            dialog,
            &mut state,
        );
    }
    if app.task_popup && app.task.is_some() {
        let dialog = dialog_rect(area, 76, 13);
        f.render_widget(Clear, dialog);
        let state = if app.control.cancelled() {
            tr("正在取消 · 等待当前操作结束")
        } else if app.paused {
            tr("已暂停")
        } else {
            tr("任务进行中")
        };
        let block = panel(state).style(Style::default().bg(BG).fg(TEXT));
        let inner = block.inner(dialog);
        f.render_widget(block, dialog);
        let sections = Layout::default()
            .constraints([
                Constraint::Length(2),
                Constraint::Length(2),
                Constraint::Length(2),
                Constraint::Min(0),
                Constraint::Length(2),
            ])
            .split(inner);
        f.render_widget(
            Paragraph::new(label.as_str()).wrap(Wrap { trim: false }),
            sections[0],
        );
        let known = app.transfer.as_ref().map_or(*total > 0, |(transfer, _)| {
            transfer.total.is_some_and(|total| total > 0)
        });
        f.render_widget(
            Gauge::default()
                .ratio(ratio)
                .gauge_style(Style::default().fg(ACCENT).bg(Color::Rgb(27, 39, 50)))
                .label(if known {
                    format!("{:.0}%", ratio * 100.0)
                } else {
                    tr("处理中 · 等待进度").into()
                }),
            sections[1],
        );
        let detail = app.transfer.as_ref().map_or_else(
            || {
                if *total > 0 {
                    tformat!("{done} / {total} 项", done = done, total = total)
                } else {
                    tr("准备或处理文件中").into()
                }
            },
            |(transfer, updated)| {
                if !app.paused && updated.elapsed() > Duration::from_secs(3) {
                    tr("等待数据或校验中").into()
                } else {
                    transfer.display(app.paused)
                }
            },
        );
        f.render_widget(
            Paragraph::new(detail).wrap(Wrap { trim: false }),
            sections[2],
        );
        f.render_widget(
            Paragraph::new(app.logs.back().map(String::as_str).unwrap_or(""))
                .style(Style::default().fg(MUTED))
                .wrap(Wrap { trim: false }),
            sections[3],
        );
        let controls = if app.control.cancelled() {
            tr("Esc 后台等待 · L 查看日志")
        } else if app.pause_allowed {
            tr("P 暂停/继续 · C 取消\nEsc 后台运行 · L 日志 · F6 任务")
        } else {
            tr("C 取消 · Esc 后台运行\nL 日志 · F6 任务")
        };
        f.render_widget(
            Paragraph::new(controls).style(Style::default().fg(ACCENT)),
            sections[4],
        );
    }
    if let Some((title, message)) = &app.task_result {
        let dialog = dialog_rect(area, 76, 12);
        f.render_widget(Clear, dialog);
        let block =
            panel(title.as_str()).title_bottom(tr(" ↑↓ / PgUp PgDn 滚动 · Enter 关闭 · L 日志 "));
        let inner = block.inner(dialog);
        let paragraph = Paragraph::new(message.as_str())
            .style(Style::default().bg(BG).fg(TEXT))
            .wrap(Wrap { trim: false });
        let maximum = paragraph
            .line_count(inner.width)
            .saturating_sub(inner.height as usize)
            .min(u16::MAX as usize) as u16;
        app.result_max_scroll.set(maximum);
        f.render_widget(
            paragraph
                .block(block)
                .scroll((app.result_scroll.min(maximum), 0)),
            dialog,
        );
    }
    if let Some(editor) = &app.editor {
        let dialog = dialog_rect(area, 88, 7);
        f.render_widget(Clear, dialog);
        let title = match editor.kind {
            EditKind::BackupWorld => tr("备份 ZIP 路径 · 不覆盖已有文件 · Enter 备份"),
            EditKind::ImportWorldZip => tr("输入世界 ZIP 路径 · Enter 下一步"),
            EditKind::ImportWorldName => tr("新存档目录名 · 不覆盖现有世界 · Enter 导入"),
            EditKind::Search => tr("搜索 · Enter 应用 · Ctrl+U 清空"),
            EditKind::Setting => tr("编辑设置 · Enter 保存 · Esc 取消"),
            EditKind::ImportMod => tr("输入本地 JAR 路径 · Enter 导入"),
            EditKind::OfflineAccount => tr("输入离线用户名 · Enter 添加并选择"),
            EditKind::ImportPack => tr("输入 Modrinth .mrpack / CurseForge ZIP 路径"),
            EditKind::NameInstance => tr("实例显示名称 · Enter 保存 · ID 保持稳定"),
            EditKind::CloneInstance => tr("新实例 ID · 复制配置、Mod 与存档"),
            EditKind::ExportPack => tr("输出 .mrpack 路径 · 不包含存档与账户"),
            EditKind::CurseKey => tr("输入自己的 CurseForge API 密钥 · 系统安全存储"),
            EditKind::ModQuery => tr("Modrinth 搜索关键词 · 按当前实例版本与加载器筛选"),
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
            .block(panel(heading).title_bottom(tr(" Enter 确认 · Esc 取消 · Ctrl+U 清空 ")))
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
            Paragraph::new(tr(
                "游戏会立即终止，未保存的进度可能丢失。\nY 确认终止 · N / Esc 返回",
            ))
            .block(panel(tr("终止游戏确认")))
            .style(Style::default().fg(Color::Yellow).bg(BG))
            .wrap(Wrap { trim: false }),
            dialog,
        );
    }
    if let Some(id) = &app.confirm_remove {
        let dialog = dialog_rect(area, 72, 7);
        f.render_widget(Clear, dialog);
        f.render_widget(
            Paragraph::new(tformat!(
                "将 {id} 移至回收站，数据可恢复。\nY 确认 · N / Esc 返回",
                id = id
            ))
            .block(panel(tr("移除实例")))
            .style(Style::default().bg(BG).fg(Color::Yellow))
            .wrap(Wrap { trim: false }),
            dialog,
        );
    }
    if let Some(selected) = app.language_picker {
        let dialog = dialog_rect(area, 78, 16);
        f.render_widget(Clear, dialog);
        let items: Vec<_> = std::iter::once(ListItem::new("System / 跟随系统 [auto]"))
            .chain(crate::localization::UiLanguage::ALL.iter().map(|language| {
                let (translated, total) = language.coverage();
                ListItem::new(format!(
                    "{}  [{}]  {translated}/{total}",
                    language.name(),
                    language.game_code()
                ))
            }))
            .collect();
        let mut state = ListState::default().with_selected(Some(selected));
        f.render_stateful_widget(
            List::new(items)
                .block(panel("Language / 语言").title_bottom(" ↑↓ · Enter Select · Esc Back "))
                .style(Style::default().bg(BG).fg(TEXT))
                .highlight_style(highlight()),
            dialog,
            &mut state,
        );
    }
    if app.help {
        let dialog = dialog_rect(area, 88, 16);
        f.render_widget(Clear, dialog);
        f.render_widget(
            Paragraph::new(tformat!("{}\n\nTab / Shift+Tab 切换导航与内容焦点\n导航 ↑↓ 选择 · Enter / → 进入内容\n内容 ← 返回导航 · 1–5 直达入口\nEnter 打开 · Esc 返回上一级\nPgUp / PgDn 翻页\n任务：F6 进度 · C 取消 · P 暂停 / 继续\nQ 空闲时退出",
                page_actions(app)))
            .block(panel(tr("当前页面 · 操作帮助")).title_bottom(tr(" ↑↓ 滚动 · F1 / Esc 返回 ")))
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
    fn multilingual_picker_navigation_and_all_language_frames() {
        let mut app = app();
        app.config.language = "auto".into();
        app.tab = 2;
        app.setting = 16;
        app.action();
        assert_eq!(app.language_picker, Some(0));
        app.key(event::KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(app.language_picker, Some(10));
        app.key(event::KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.language_picker, Some(0));
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
        for language in crate::localization::UiLanguage::ALL {
            app.config.language = language.game_code().into();
            terminal.draw(|f| draw(f, &app)).unwrap();
            app.language_picker = None;
            for tab in [0, 2, 4, 6, 8] {
                app.tab = tab;
                terminal.draw(|f| draw(f, &app)).unwrap();
            }
            app.language_picker = Some(0);
        }
        app.key(event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.language_picker.is_none());
        assert_eq!(app.config.language, "lzh");
        app.tab = 2;
        app.scope = Some("1.21.1".into());
        app.action();
        assert!(app.language_picker.is_none());
    }
    #[test]
    fn english_pages_translate_controls_and_preserve_user_names() {
        let mut app = app();
        app.config.language = "en_us".into();
        app.names.insert("1.21.1".into(), "账户".into());
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(140, 44)).unwrap();
        for (tab, expected) in [
            (0, "Instances"),
            (2, "Launchsettings"),
            (4, "Accounts"),
            (8, "Globalgamesettings"),
        ] {
            app.tab = tab;
            let rendered = terminal.draw(|f| draw(f, &app)).unwrap();
            let text = rendered
                .buffer
                .content
                .iter()
                .map(|c| c.symbol())
                .collect::<String>();
            assert!(
                text.replace(' ', "").contains(expected),
                "missing {expected}"
            );
            assert!(!text.contains("↑↓ 选择"));
            if tab == 0 {
                assert!(text.replace(' ', "").contains("账户"));
            }
        }
        app.config.language = "zh_cn".into();
        app.tab = 0;
        let rendered = terminal.draw(|f| draw(f, &app)).unwrap();
        let text = rendered
            .buffer
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(text.replace(' ', "").contains("实例列表"));
    }
    #[test]
    fn responsive_pages_and_dialogs_fit() {
        let mut app = app();
        for (width, height) in [(1, 1), (48, 16), (70, 20), (100, 30), (160, 45)] {
            let mut terminal =
                Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            for tab in 0..10 {
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
                assert!(text.contains("账户"));
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
    fn instance_settings_reject_global_fields_before_editing() {
        let mut app = app();
        app.tab = 2;
        app.scope = Some("test".into());
        for index in [0, 3, 7, 10, 11, 12, 16] {
            app.setting = index;
            app.action();
            assert!(app.editor.is_none());
            assert!(app.language_picker.is_none());
        }
    }
    #[test]
    fn settings_groups_navigate_only_their_own_fields() {
        let mut covered = Vec::new();
        for (group, indices) in SETTING_GROUPS.iter().enumerate() {
            let mut app = app();
            app.tab = 8;
            app.selection = group;
            app.action();
            assert_eq!(app.tab, 2);
            assert_eq!(app.setting, indices[0]);
            assert_eq!(app.count(), indices.len());
            app.key(event::KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
            assert_eq!(app.setting, *indices.last().unwrap());
            app.key(event::KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
            assert_eq!(app.setting, indices[0]);
            app.key(event::KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
            assert_eq!(app.setting, *indices.last().unwrap());
            app.key(event::KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
            assert_eq!(app.setting, indices[0]);
            app.key(event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            assert_eq!(app.tab, 8);
            assert_eq!(app.selection, group);
            covered.extend_from_slice(indices);
        }
        covered.sort_unstable();
        assert_eq!(covered, (0..SETTINGS.len()).collect::<Vec<_>>());
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
        app.key(event::KeyEvent::new(KeyCode::Right, KeyModifiers::ALT));
        assert_eq!(app.tab, 6);
        assert_eq!(app.log_horizontal, 8);
        app.key(event::KeyEvent::new(KeyCode::Left, KeyModifiers::ALT));
        assert_eq!(app.log_horizontal, 0);
    }
    #[test]
    fn sidebar_focus_routes_arrows_without_moving_content_selection() {
        let mut app = app();
        app.selection = 1;
        app.key(event::KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert!(app.nav_focused);
        app.key(event::KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.tab, 7);
        assert_eq!(
            app.managed_instance.as_deref(),
            Some("fabric-loader-1.20.1")
        );
        app.key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!app.nav_focused);
        assert_eq!(app.tab, 7);
        assert!(app.task.is_none());
        app.key(event::KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.selection, 1);
        app.key(event::KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(app.nav_focused);
        app.key(event::KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(!app.nav_focused);
        assert_eq!(app.selection, 1);
        app.key(event::KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        app.key(event::KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(app.tab, 8);
        app.key(event::KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert!(!app.nav_focused);
        app.key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.tab, 2);
        app.key(event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.tab, 8);
        assert!(!app.nav_focused);
    }
    #[test]
    fn instance_navigation_keeps_identity_and_returns_to_filtered_list() {
        let mut app = app();
        app.query = "fabric".into();
        app.key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.tab, 7);
        assert_eq!(
            app.managed_instance.as_deref(),
            Some("fabric-loader-1.20.1")
        );
        app.selection = 1;
        app.key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.tab, 2);
        assert_eq!(app.scope.as_deref(), Some("fabric-loader-1.20.1"));
        app.key(event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!((app.tab, app.selection), (7, 1));
        app.selection = 2;
        app.key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.tab, 3);
        assert_eq!(app.mod_instance.as_deref(), Some("fabric-loader-1.20.1"));
        app.key(event::KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!((app.tab, app.selection), (7, 2));
        app.selection = 9;
        app.key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.editor_instance.as_deref(), Some("fabric-loader-1.20.1"));
        app.key(event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.tab, 7);
        app.key(event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.tab, 0);
        assert_eq!(app.query, "fabric");
        assert_eq!(app.selected().as_deref(), Some("fabric-loader-1.20.1"));
        assert!(app.task.is_none());
        app.key(event::KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE));
        assert_eq!(app.tab, 1);
        assert!(app.query.is_empty());
        app.query = "1.21".into();
        app.key(event::KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
        assert_eq!(app.query, "fabric");
        app.key(event::KeyEvent::new(KeyCode::Char('4'), KeyModifiers::NONE));
        assert_eq!(app.query, "1.21");
    }
    #[test]
    fn primary_navigation_hides_tools_and_preserves_current_instance() {
        let mut app = app();
        app.selection = 1;
        app.key(event::KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
        assert_eq!(app.tab, 4);
        app.key(event::KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
        assert_eq!(app.tab, 7);
        assert_eq!(
            app.managed_instance.as_deref(),
            Some("fabric-loader-1.20.1")
        );
        app.selection = 4;
        app.key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.tab, 9);
        app.key(event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!((app.tab, app.selection), (7, 4));
        app.key(event::KeyEvent::new(KeyCode::Char('5'), KeyModifiers::NONE));
        assert_eq!(app.tab, 8);
        app.selection = 6;
        app.key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.tab, 6);
        app.key(event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!((app.tab, app.selection), (8, 6));
        app.selection = 0;
        app.key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.tab, 2);
        assert!(app.scope.is_none());
        assert_eq!(app.primary_index(), 4);
    }
    #[test]
    fn missing_instance_management_returns_to_list_without_starting_a_task() {
        let mut app = app();
        app.managed_instance = Some("removed".into());
        app.tab = 7;
        app.key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.tab, 0);
        assert!(app.task.is_none());
    }
    #[test]
    fn loader_list_handoff_does_not_require_dismissing_a_result() {
        let mut app = app();
        app.task = Some(WorkKind::Other);
        app.task_popup = true;
        app.tx
            .send(Message::LoaderVersions {
                kind: crate::loader_catalog::InstallerKind::Fabric,
                game: "1.21.1".into(),
                versions: vec![],
            })
            .unwrap();
        app.messages();
        app.tx.send(Message::Done(Ok("列表已获取".into()))).unwrap();
        app.messages();
        assert!(app.loader_picker.is_some());
        assert!(app.task_result.is_none());
        assert!(!app.task_popup);
        app.key(event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.loader_picker.is_none());
    }
    #[test]
    fn long_result_scrolls_to_the_last_line_and_clamps() {
        let mut app = app();
        let message = format!("{}\nEND-OF-ERROR", "很长的中文错误信息。".repeat(150));
        app.task_result = Some(("任务失败".into(), message));
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        assert!(app.result_max_scroll.get() > 0);
        app.key(event::KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        let frame = terminal.draw(|f| draw(f, &app)).unwrap();
        let text: String = frame
            .buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("END-OF-ERROR"));
        app.key(event::KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.result_scroll, app.result_max_scroll.get());
        app.key(event::KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(app.result_scroll, 0);
    }
    #[test]
    fn repeated_navigation_works_but_confirmation_does_not_repeat() {
        let mut app = app();
        app.tab = 2;
        app.key(event::KeyEvent::new_with_kind(
            KeyCode::Down,
            KeyModifiers::NONE,
            KeyEventKind::Repeat,
        ));
        assert_eq!(app.setting, 1);
        app.task = Some(WorkKind::Game);
        app.confirm_stop = true;
        app.key(event::KeyEvent::new_with_kind(
            KeyCode::Char('y'),
            KeyModifiers::NONE,
            KeyEventKind::Repeat,
        ));
        assert!(!app.control.cancelled());
        app.key(event::KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(app.control.cancelled());
    }
    #[test]
    fn hidden_world_task_can_pause_cancel_and_close_picker() {
        let mut app = app();
        app.task = Some(WorkKind::Download);
        app.pause_allowed = true;
        app.world_picker = Some(WorldPicker {
            instance: "1.21.1".into(),
            worlds: vec![],
            selected: 0,
        });
        app.key(event::KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        assert!(app.paused);
        app.key(event::KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        assert!(app.control.cancelled());
        assert!(!app.paused);
        app.key(event::KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        assert!(!app.paused);
        app.key(event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.world_picker.is_none());
        assert!(app.task.is_some());
    }
    #[test]
    fn task_completion_is_visible_and_can_open_logs() {
        let mut app = app();
        app.task = Some(WorkKind::Download);
        app.task_popup = true;
        app.tx
            .send(Message::Done(Ok("资源安装完成".into())))
            .unwrap();
        app.messages();
        assert!(app.task.is_none());
        assert!(!app.task_popup);
        assert_eq!(app.task_result.as_ref().unwrap().0, "任务已完成");
        app.key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.task_result.is_none());
        app.task = Some(WorkKind::Download);
        app.tx
            .send(Message::Done(Err("文件校验失败".into())))
            .unwrap();
        app.messages();
        assert_eq!(app.task_result.as_ref().unwrap().0, "任务失败");
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        app.key(event::KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
        assert!(app.task_result.is_none());
        assert_eq!(app.tab, 6);
    }
    #[test]
    fn task_popup_controls_and_layout() {
        let mut app = app();
        app.task = Some(WorkKind::Download);
        app.pause_allowed = true;
        app.progress = ("下载游戏资源".into(), 3, 10);
        app.key(event::KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE));
        assert!(app.task_popup);
        app.key(event::KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        assert!(app.paused);
        for (width, height) in [(100, 30), (60, 18), (40, 12)] {
            let mut terminal =
                Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| draw(f, &app)).unwrap();
        }
        app.key(event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.task_popup);
        assert!(!app.control.cancelled());
        app.key(event::KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE));
        app.key(event::KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        assert!(app.control.cancelled());
        assert!(!app.paused);
        app.key(event::KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
        assert_eq!(app.tab, 6);
        assert!(!app.task_popup);
    }
    #[test]
    fn log_history_stays_anchored_and_pages_use_visible_height() {
        let mut app = app();
        app.tab = 6;
        app.logs.clear();
        for index in 0..30 {
            app.log(format!("line {index}"));
        }
        app.log_view_height.set(8);
        app.key(event::KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(app.log_scroll, 8);
        let end = app.logs.len() - app.log_scroll;
        let anchor = app.logs[end - 1].clone();
        app.log("new line");
        assert_eq!(app.logs[app.logs.len() - app.log_scroll - 1], anchor);
        app.key(event::KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(app.logs.len() - app.log_scroll, 8);
        app.key(event::KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        app.log("follow latest");
        assert_eq!(app.log_scroll, 0);
        for _ in 0..600 {
            app.log("bounded");
        }
        app.key(event::KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        app.log("evict oldest");
        assert_eq!(app.logs.len(), 500);
        assert_eq!(app.log_scroll, 492);
    }
    #[test]
    fn log_arrows_scroll_and_leave_page() {
        let mut app = app();
        app.tab = 6;
        app.logs = vec!["first".into(), "second".into(), "third".into()].into();
        for _ in 0..4 {
            app.key(event::KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        }
        assert_eq!(app.log_scroll, 2);
        app.key(event::KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.log_scroll, 1);
        app.key(event::KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        app.key(event::KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.log_scroll, 0);
        app.key(event::KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(app.tab, 6);
        assert!(app.nav_focused);
        app.key(event::KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(app.tab, 6);
        assert!(!app.nav_focused);
        app.logs.clear();
        app.key(event::KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.log_scroll, 0);
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
        let mut app = App::new(
            Config {
                language: "zh_cn".into(),
                ..Config::default()
            },
            tx,
            rx,
        );
        app.nav_focused = false;
        app.local = vec!["1.21.1".into(), "fabric-loader-1.20.1".into()];
        app
    }
    #[test]
    fn all_pages_render() {
        let mut app = app();
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        for (tab, title) in [
            "实例列表",
            "官方版本",
            "启动设置",
            "Mod管理",
            "账户管理",
            "实例回收站",
            "任务日志",
            "实例管理",
            "设置",
            "加载器/组件",
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

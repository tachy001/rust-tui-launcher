mod theme;
mod view;

use crate::localization::tr;
use crate::{
    accounts::Accounts,
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
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, layout::Rect, Terminal};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
    io,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::Duration,
};
use view::draw;

// ── Top-level & Sub-page Enums ─────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PrimaryPage {
    Play,
    Downloads,
    Accounts,
    Settings,
    Logs,
}

impl PrimaryPage {
    pub const ALL: [Self; 5] = [
        Self::Play,
        Self::Downloads,
        Self::Accounts,
        Self::Settings,
        Self::Logs,
    ];

    pub fn index(self) -> usize {
        match self {
            Self::Play => 0,
            Self::Downloads => 1,
            Self::Accounts => 2,
            Self::Settings => 3,
            Self::Logs => 4,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Play => "启动与实例",
            Self::Downloads => "下载中心",
            Self::Accounts => "账户管理",
            Self::Settings => "全局设置",
            Self::Logs => "任务日志",
        }
    }

    pub fn short_label(self) -> &'static str {
        match self {
            Self::Play => "实例",
            Self::Downloads => "下载",
            Self::Accounts => "账户",
            Self::Settings => "设置",
            Self::Logs => "日志",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PaneFocus {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WorkbenchTab {
    Overview,
    Settings,
    Mods,
    Worlds,
    Loaders,
}

impl WorkbenchTab {
    pub const ALL: [Self; 5] = [
        Self::Overview,
        Self::Settings,
        Self::Mods,
        Self::Worlds,
        Self::Loaders,
    ];

    pub fn index(self) -> usize {
        match self {
            Self::Overview => 0,
            Self::Settings => 1,
            Self::Mods => 2,
            Self::Worlds => 3,
            Self::Loaders => 4,
        }
    }

    pub fn next(self) -> Self {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    pub fn prev(self) -> Self {
        Self::ALL[(self.index() + Self::ALL.len() - 1) % Self::ALL.len()]
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Overview => "概览",
            Self::Settings => "独立设置",
            Self::Mods => "Mod",
            Self::Worlds => "存档",
            Self::Loaders => "组件",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HitTarget {
    Page(PrimaryPage),
    WorkbenchTab(WorkbenchTab),
    FocusPane(PaneFocus),
    LaunchNow,
    ListArea {
        pane: PaneFocus,
        offset: usize,
        row_height: u16,
        count: usize,
    },
    TaskTogglePopup,
}

// ── Static Definitions ─────────────────────────────────────────────────────

pub(super) const OVERVIEW_ACTIONS: [(&str, &str, &str); 9] = [
    (
        "▸",
        "启动游戏",
        "使用当前选中账户立即启动此实例（快捷键 Space）",
    ),
    (
        "✓",
        "检查并修复",
        "校验游戏核心、依赖库与资源完整性并补全缺失文件",
    ),
    (
        "◆",
        "安装建议 Java",
        "自动匹配并下载 Mojang 官方 Java 运行时",
    ),
    (
        "⊕",
        "启动诊断",
        "分析最近一次运行与崩溃输出，定位常见故障原因",
    ),
    ("≡", "游戏日志", "在日志视图中读取该实例最近一次运行输出"),
    (
        "◇",
        "修改名称",
        "设置自定义显示名称，底层版本 ID 与继承保持稳定",
    ),
    ("+", "复制实例", "将实例配置、Mod 与存档完整克隆为新实例"),
    (
        "→",
        "导出整合包",
        "打包导出为 Modrinth .mrpack（不含存档与凭证）",
    ),
    (
        "×",
        "移入回收站",
        "安全移入启动器回收站，随时可在全局设置中恢复",
    ),
];

pub(super) const DOWNLOAD_ACTIONS: [(&str, &str, &str); 7] = [
    (
        "↓",
        "安装纯净原版",
        "Enter · 下载并校验官方原版核心与资源文件",
    ),
    (
        "◈",
        "安装 Fabric",
        "F · 轻量迅速的现代加载器，先选版本再安装",
    ),
    (
        "◈",
        "安装 NeoForge",
        "N · 新版本主流社区加载器，自动处理安装器流程",
    ),
    (
        "◈",
        "安装 Forge",
        "H · 经典大型模组生态加载器，支持新旧版本",
    ),
    ("◈", "安装 Quilt", "U · 兼容 Fabric 生态的社区分支加载器"),
    ("◈", "安装 OptiFine", "O · 高清修复与光影支持组件"),
    (
        "+",
        "导入整合包",
        "I · 从本地导入 .mrpack 或 CurseForge .zip",
    ),
];

pub(super) const ACCOUNT_ACTIONS: [(&str, &str, &str); 5] = [
    (
        "●",
        "切换为当前账户",
        "Enter · 将左侧所选账户设为默认启动身份",
    ),
    (
        "◆",
        "微软正版登录",
        "A · 打开系统浏览器完成正版授权，凭证安全存储",
    ),
    (
        "+",
        "添加离线账户",
        "N · 创建单机与离线模式使用的本地角色名",
    ),
    (
        "◈",
        "CurseForge 密钥",
        "Shift+K · 将个人 API 密钥存入系统安全凭据库",
    ),
    ("×", "移除所选账户", "D · 删除账户记录并清除本机关联令牌"),
];

pub(super) const LOADER_ITEMS: [(&str, &str); 5] = [
    (
        "Fabric",
        "轻量、更新迅速的现代 Mod 加载器，适合主流新版本。",
    ),
    ("Quilt", "兼容绝大多数 Fabric 模组的社区分支加载器。"),
    ("Forge", "历史悠久、经典大型整合包生态最丰富的加载器。"),
    ("NeoForge", "Forge 现代社区演进分支，1.20.1+ 模组主流选择。"),
    ("OptiFine", "画质细节调节、性能优化与光影包支持组件。"),
];

pub(super) const SETTINGS_CATEGORIES: [(&str, &str); 6] = [
    (
        "游戏与启动",
        "内存配额、版本隔离、窗口分辨率、快速进入与游戏语言",
    ),
    (
        "Java 与兼容",
        "Java 运行时路径、本机环境探测、附加参数与原生库修补",
    ),
    (
        "下载与整合包",
        "镜像下载源切换、并发线程数与整合包可选文件策略",
    ),
    (
        "通用与语言",
        "界面多语言切换、默认离线名称、游戏根目录与应用 ID",
    ),
    ("全部设置", "在同一列表中浏览与修改所有 21 项启动器全局参数"),
    ("实例回收站", "查看已移除的游戏实例并原位恢复全部实例数据"),
];

pub(super) const SETTING_GROUPS: [&[usize]; 4] = [
    &[19, 2, 20, 4, 8, 9, 13, 14, 17, 15],
    &[1, 18, 5, 6],
    &[11, 7, 12],
    &[16, 0, 3, 10],
];

pub(super) const INSTANCE_SETTING_INDICES: [usize; 14] =
    [1, 19, 2, 20, 4, 8, 9, 13, 14, 17, 15, 5, 6, 18];

pub(super) const SETTINGS: [&str; 21] = [
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

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum WorkKind {
    Download,
    Game,
    Other,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum EditKind {
    InstanceSearch,
    RemoteSearch,
    GlobalSetting(usize),
    InstanceSetting(usize),
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

pub(super) struct Editor {
    pub kind: EditKind,
    pub text: String,
}

impl Drop for Editor {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.text);
    }
}

pub(super) struct LoaderPicker {
    pub kind: crate::loader_catalog::InstallerKind,
    pub game: String,
    pub versions: Vec<crate::loader_catalog::LoaderVersion>,
    pub selected: usize,
}

// ── Main Application State ─────────────────────────────────────────────────

pub(super) struct App {
    pub config: Config,
    pub page: PrimaryPage,
    pub focus: PaneFocus,
    pub return_page: Option<(PrimaryPage, PaneFocus)>,

    // Play page (Left: instances, Right: Workbench)
    pub local: Vec<String>,
    pub names: HashMap<String, String>,
    pub instance_sel: usize,
    pub instance_query: String,
    pub managed_instance: Option<String>,
    pub wb_tab: WorkbenchTab,
    pub wb_overview_sel: usize,
    pub wb_setting_sel: usize,
    pub wb_loader_sel: usize,

    // Instance Mods & Worlds (live synced for `managed_instance`)
    pub mod_list: Vec<ModEntry>,
    pub mod_sel: usize,
    pub mod_search: Option<crate::modrinth::SearchResult>,
    pub mod_versions: Option<(String, Vec<crate::modrinth::ModVersion>)>,
    pub mod_query: String,
    pub mod_updates_only: bool,
    pub world_list: Vec<String>,
    pub world_sel: usize,
    pub pending_world_zip: Option<PathBuf>,

    // Downloads page (Left: remote catalog, Right: installer studio)
    pub remote: Vec<RemoteVersion>,
    pub remote_sel: usize,
    pub remote_query: String,
    pub snapshots: bool,
    pub download_action_sel: usize,

    // Accounts page (Left: accounts, Right: account actions)
    pub accounts: Accounts,
    pub account_sel: usize,
    pub account_action_sel: usize,

    // Settings page (Left: categories, Right: fields or trash)
    pub settings_cat: usize,
    pub settings_field_sel: usize,
    pub trash: Vec<crate::instances::TrashEntry>,
    pub trash_sel: usize,

    // Logs & Background Tasks
    pub logs: VecDeque<String>,
    pub log_scroll: usize,
    pub log_view_height: Cell<usize>,
    pub log_horizontal: u16,
    pub task: Option<WorkKind>,
    pub control: TaskControl,
    pub progress: (String, usize, usize),
    pub transfer: Option<(crate::download_progress::Snapshot, std::time::Instant)>,
    pub paused: bool,
    pub pause_allowed: bool,
    pub task_popup: bool,
    pub task_handoff: bool,
    pub task_result: Option<(String, String)>,
    pub result_scroll: u16,
    pub result_max_scroll: Cell<u16>,

    // Modals & Pickers
    pub editor: Option<Editor>,
    pub editor_instance: Option<String>,
    pub loader_picker: Option<LoaderPicker>,
    pub language_picker: Option<usize>,
    pub confirm_stop: bool,
    pub confirm_remove: Option<String>,
    pub help: bool,
    pub help_scroll: u16,

    // Channels & Mouse Hit Map
    pub tx: Sender<Message>,
    pub rx: Receiver<Message>,
    pub hits: RefCell<Vec<(Rect, HitTarget)>>,
}

impl App {
    fn new(config: Config, tx: Sender<Message>, rx: Receiver<Message>) -> Self {
        crate::localization::set(&config.language);
        let local = backend::installed(&config);
        let instance_sel = config
            .last_version
            .as_ref()
            .and_then(|id| local.iter().position(|v| v == id))
            .unwrap_or(0);
        let names = crate::instances::names(&config);
        let managed_instance = local.get(instance_sel).cloned();
        let accounts = Accounts::load().unwrap_or_default();
        let account_sel = accounts
            .active
            .as_ref()
            .and_then(|active| accounts.entries.iter().position(|a| &a.id == active))
            .unwrap_or(0);

        let mut app = Self {
            config,
            page: PrimaryPage::Play,
            focus: PaneFocus::Left,
            return_page: None,
            local,
            names,
            instance_sel,
            instance_query: String::new(),
            managed_instance,
            wb_tab: WorkbenchTab::Overview,
            wb_overview_sel: 0,
            wb_setting_sel: 0,
            wb_loader_sel: 0,
            mod_list: Vec::new(),
            mod_sel: 0,
            mod_search: None,
            mod_versions: None,
            mod_query: String::new(),
            mod_updates_only: false,
            world_list: Vec::new(),
            world_sel: 0,
            pending_world_zip: None,
            remote: Vec::new(),
            remote_sel: 0,
            remote_query: String::new(),
            snapshots: false,
            download_action_sel: 0,
            accounts,
            account_sel,
            account_action_sel: 0,
            settings_cat: 0,
            settings_field_sel: 0,
            trash: Vec::new(),
            trash_sel: 0,
            logs: VecDeque::new(),
            log_scroll: 0,
            log_view_height: Cell::new(1),
            log_horizontal: 0,
            task: None,
            control: TaskControl::default(),
            progress: (tr("空闲").into(), 0, 0),
            transfer: None,
            paused: false,
            pause_allowed: true,
            task_popup: false,
            task_handoff: false,
            task_result: None,
            result_scroll: 0,
            result_max_scroll: Cell::new(0),
            editor: None,
            editor_instance: None,
            loader_picker: None,
            language_picker: None,
            confirm_stop: false,
            confirm_remove: None,
            help: false,
            help_scroll: 0,
            tx,
            rx,
            hits: RefCell::new(Vec::new()),
        };
        app.sync_managed_data();
        app
    }

    fn load_cached_catalog(&mut self) {
        use crate::metadata_cache::{FileMetadataStore, MetadataStore};
        let store_dir = Path::new(".tui-mc");
        if !store_dir.exists() {
            return;
        }
        if let Ok(Some(bytes)) = FileMetadataStore::new(store_dir).read(crate::metadata::MANIFEST) {
            if let Ok(manifest) = serde_json::from_slice::<crate::metadata::Manifest>(&bytes) {
                self.remote = manifest.versions;
            }
        }
    }

    pub(super) fn log(&mut self, text: impl Into<String>) {
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

    fn report(&mut self, result: Result<String>) {
        match result {
            Ok(s) => self.log(s),
            Err(e) => self.log(tformat!("错误 · {e:#}", e = e)),
        }
    }

    pub(super) fn show_logs(&mut self) {
        if self.page != PrimaryPage::Logs {
            self.return_page = Some((self.page, self.focus));
            self.page = PrimaryPage::Logs;
            self.focus = PaneFocus::Left;
        }
        self.log_scroll = 0;
    }

    pub(super) fn switch_page(&mut self, next: PrimaryPage) {
        if self.page == PrimaryPage::Play {
            if let Some(id) = self.selected_instance() {
                self.managed_instance = Some(id);
            }
        }
        self.return_page = None;
        self.page = next;
        self.focus = PaneFocus::Left;
        if next == PrimaryPage::Play {
            self.restore_instance_selection();
            self.sync_managed_data();
        } else if next == PrimaryPage::Settings && self.settings_cat == 5 {
            self.refresh_trash();
        }
    }

    fn restore_instance_selection(&mut self) {
        let visible: Vec<String> = self.local_visible().into_iter().cloned().collect();
        if let Some(pos) = visible
            .iter()
            .position(|id| Some(id.as_str()) == self.managed_instance.as_deref())
        {
            self.instance_sel = pos;
        } else {
            self.instance_sel = self.instance_sel.min(visible.len().saturating_sub(1));
            self.managed_instance = visible.get(self.instance_sel).cloned();
        }
    }

    pub(super) fn sync_selection_to_managed(&mut self) {
        let next = self.selected_instance();
        if next != self.managed_instance {
            self.managed_instance = next;
            self.mod_search = None;
            self.mod_versions = None;
            self.mod_updates_only = false;
            self.mod_sel = 0;
            self.world_sel = 0;
            self.sync_managed_data();
        }
    }

    pub(super) fn sync_managed_data(&mut self) {
        self.mod_list.clear();
        self.world_list.clear();
        let Some(id) = self.managed_instance.clone() else {
            return;
        };
        if !self.local.contains(&id) {
            return;
        }
        if let Ok(mods) = mods::list(&self.config, &id) {
            self.mod_list = mods;
        }
        if let Ok(worlds) = crate::worlds::list(&self.config, &id) {
            self.world_list = worlds;
        }
        self.mod_sel = self.mod_sel.min(self.mod_item_count().saturating_sub(1));
        self.world_sel = self.world_sel.min(self.world_list.len().saturating_sub(1));
    }

    pub(super) fn local_visible(&self) -> Vec<&String> {
        let q = self.instance_query.to_lowercase();
        self.local
            .iter()
            .filter(|id| {
                q.is_empty()
                    || id.to_lowercase().contains(&q)
                    || self
                        .names
                        .get(*id)
                        .is_some_and(|name| name.to_lowercase().contains(&q))
            })
            .collect()
    }

    pub(super) fn remote_visible(&self) -> Vec<&RemoteVersion> {
        let q = self.remote_query.to_lowercase();
        self.remote
            .iter()
            .filter(|v| {
                (self.snapshots || v.kind == "release")
                    && (q.is_empty() || v.id.to_lowercase().contains(&q))
            })
            .collect()
    }

    pub(super) fn selected_instance(&self) -> Option<String> {
        self.local_visible()
            .get(self.instance_sel)
            .map(|id| (*id).clone())
    }

    pub(super) fn active_instance(&self) -> Option<String> {
        self.managed_instance
            .clone()
            .filter(|id| self.local.contains(id))
            .or_else(|| self.selected_instance())
    }

    pub(super) fn selected_remote(&self) -> Option<RemoteVersion> {
        self.remote_visible()
            .get(self.remote_sel)
            .map(|v| (*v).clone())
    }

    pub(super) fn mod_item_count(&self) -> usize {
        if let Some((_, versions)) = &self.mod_versions {
            versions.len()
        } else if let Some(search) = &self.mod_search {
            search.hits.len()
        } else {
            self.mod_list.len()
        }
    }

    pub(super) fn global_setting_indices(&self) -> Vec<usize> {
        match self.settings_cat {
            0..=3 => SETTING_GROUPS[self.settings_cat].to_vec(),
            4 => (0..SETTINGS.len()).collect(),
            _ => Vec::new(),
        }
    }

    pub(super) fn active_global_setting_index(&self) -> Option<usize> {
        let indices = self.global_setting_indices();
        indices.get(self.settings_field_sel).copied()
    }

    pub(super) fn active_instance_setting_index(&self) -> usize {
        INSTANCE_SETTING_INDICES[self.wb_setting_sel.min(INSTANCE_SETTING_INDICES.len() - 1)]
    }

    pub(super) fn is_instance_overridden(&self, id: &str, index: usize) -> bool {
        let Some(s) = self.config.instances.get(id) else {
            return false;
        };
        match index {
            1 => s.java.is_some(),
            2 => s.memory_mb.is_some(),
            4 => s.isolated.is_some(),
            5 => s.jvm_args.is_some(),
            6 => s.game_args.is_some(),
            8 => s.width.is_some(),
            9 => s.height.is_some(),
            13 => s.fullscreen.is_some(),
            14 => s.server.is_some(),
            15 => s.game_language.is_some(),
            17 => s.quick_play_world.is_some(),
            18 => s.patch_natives.is_some(),
            19 => s.auto_memory.is_some(),
            20 => s.min_memory_mb.is_some(),
            _ => false,
        }
    }

    pub(super) fn setting_value(&self, scope: Option<&str>, index: usize) -> String {
        let c = match scope
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

    fn save_setting(&mut self, scope: Option<&str>, index: usize, value: &str) -> Result<()> {
        let mut candidate = self.config.clone();
        if let Some(id) = scope {
            let instance = candidate.instances.entry(id.to_owned()).or_default();
            match index {
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
                _ => bail!("此项为全局设置，请在全局设置页修改"),
            }
            crate::instances::save_settings(&candidate, id)?;
        } else {
            match index {
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
            candidate.save()?;
        }
        self.config = candidate;
        crate::localization::set(&self.config.language);
        self.local = backend::installed(&self.config);
        self.names = crate::instances::names(&self.config);
        self.sync_managed_data();
        self.log(tr("设置已保存"));
        Ok(())
    }

    // ── Background Worker & Domain Operations ──────────────────────────────

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
        self.restore_instance_selection();
        self.sync_managed_data();
        match self.page {
            PrimaryPage::Downloads => {
                self.fetch_catalog();
            }
            PrimaryPage::Settings if self.settings_cat == 5 => {
                self.refresh_trash();
                self.log(tr("回收站列表已刷新"));
            }
            _ => {
                self.log(tr("本地版本已刷新"));
            }
        }
    }

    fn fetch_catalog(&mut self) {
        self.work(WorkKind::Download, |b| {
            b.catalog()?;
            Ok("版本目录已更新".into())
        });
    }

    fn refresh_trash(&mut self) {
        match crate::instances::trash_list(&self.config) {
            Ok(entries) => {
                self.trash = entries;
                self.trash_sel = self.trash_sel.min(self.trash.len().saturating_sub(1));
            }
            Err(e) => self.log(e.to_string()),
        }
    }

    fn launch_instance(&mut self, id: String) {
        if self.task.is_some() {
            self.log(tr("请等待当前任务完成"));
            return;
        }
        self.config.last_version = Some(id.clone());
        if let Err(e) = self.config.save() {
            self.log(e.to_string());
            return;
        }
        let config = self.config.clone();
        self.work(WorkKind::Game, move |b| {
            b.launch(&config, &id)?;
            Ok("游戏已退出".into())
        });
    }

    fn trigger_overview_action(&mut self, index: usize) {
        let Some(id) = self.active_instance() else {
            self.log(tr("请先在实例列表选择一个已安装实例"));
            self.focus = PaneFocus::Left;
            return;
        };
        if self.task.is_some() && !matches!(index, 4) {
            self.task_popup = true;
            return;
        }
        match index {
            0 => self.launch_instance(id),
            1 => {
                let config = self.config.clone();
                self.work(WorkKind::Download, move |b| {
                    b.repair(&config, &id)?;
                    Ok(tformat!("{id} 已校验修复", id = id))
                });
            }
            2 => {
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
            3 => match crate::diagnostics::analyze(&self.config, &id, None) {
                Ok(diagnosis) => {
                    let summary = diagnosis.summary();
                    for line in summary.lines() {
                        self.log(line);
                    }
                    self.task_result = Some((tr("启动诊断").into(), summary));
                    self.result_scroll = 0;
                }
                Err(error) => self.log(tformat!("诊断失败：{error}", error = error)),
            },
            4 => {
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
                        self.show_logs();
                    }
                    Err(e) => self.log(tformat!("读取日志失败：{e}", e = e)),
                }
            }
            5 => {
                self.editor_instance = Some(id.clone());
                self.editor = Some(Editor {
                    kind: EditKind::NameInstance,
                    text: self.names.get(&id).cloned().unwrap_or(id),
                });
            }
            6 => {
                self.editor_instance = Some(id.clone());
                self.editor = Some(Editor {
                    kind: EditKind::CloneInstance,
                    text: format!("{id}-copy"),
                });
            }
            7 => {
                self.editor_instance = Some(id.clone());
                self.editor = Some(Editor {
                    kind: EditKind::ExportPack,
                    text: format!("{id}.mrpack"),
                });
            }
            8 => {
                self.confirm_remove = Some(id);
            }
            _ => {}
        }
    }

    fn edit_or_toggle_setting(&mut self, scope: Option<String>, index: usize, quick_cycle: bool) {
        if self.task.is_some() {
            self.log(tr("请等待任务完成再修改设置"));
            return;
        }
        let current = self.setting_value(scope.as_deref(), index);
        if matches!(index, 4 | 12 | 13 | 18 | 19) {
            let result = parse_bool(&current).and_then(|val| {
                self.save_setting(scope.as_deref(), index, if val { "false" } else { "true" })
            });
            if let Err(err) = result {
                self.log(format!("{err:#}"));
            }
            return;
        }
        if index == 16 && scope.is_none() {
            let language = crate::localization::UiLanguage::parse(&self.config.language).ok();
            let pos = if crate::localization::UiLanguage::is_system(&self.config.language) {
                0
            } else {
                crate::localization::UiLanguage::ALL
                    .iter()
                    .position(|v| Some(*v) == language)
                    .map(|i| i + 1)
                    .unwrap_or(0)
            };
            self.language_picker = Some(pos);
            return;
        }
        if quick_cycle {
            if index == 11 && scope.is_none() {
                let next = if current == "official" {
                    "bmclapi"
                } else {
                    "official"
                };
                if let Err(err) = self.save_setting(None, 11, next) {
                    self.log(format!("{err:#}"));
                }
                return;
            }
            if index == 15 {
                let next = match current.as_str() {
                    "launcher" => "zh_cn",
                    "zh_cn" => "en_us",
                    "en_us" => "keep",
                    _ => "launcher",
                };
                if let Err(err) = self.save_setting(scope.as_deref(), 15, next) {
                    self.log(format!("{err:#}"));
                }
                return;
            }
        }
        let kind = if scope.is_some() {
            EditKind::InstanceSetting(index)
        } else {
            EditKind::GlobalSetting(index)
        };
        self.editor_instance = scope;
        self.editor = Some(Editor {
            kind,
            text: current,
        });
    }

    fn choose_loader(&mut self, kind: crate::loader_catalog::InstallerKind, from_remote: bool) {
        let game = if from_remote {
            let Some(remote) = self.selected_remote() else {
                if self.remote.is_empty() {
                    self.fetch_catalog();
                }
                return;
            };
            remote.id
        } else {
            let Some(id) = self.active_instance() else {
                self.log(tr("请先在实例列表选择一个已安装实例"));
                return;
            };
            match self
                .config
                .root()
                .and_then(|root| version::resolve(&root, &id))
            {
                Ok(resolved) => resolved.chain[0].clone(),
                Err(e) => {
                    self.log(e.to_string());
                    return;
                }
            }
        };
        self.work(WorkKind::Other, move |b| {
            let versions = b.loader_versions(kind, &game)?;
            let _ = b.tx.send(Message::LoaderVersions {
                kind,
                game,
                versions,
            });
            Ok("选择加载器版本后按 Enter 安装，Esc 取消".into())
        });
    }

    fn install_loader_shortcut(&mut self, loader: Loader, from_remote: bool) {
        self.choose_loader(
            match loader {
                Loader::Fabric => crate::loader_catalog::InstallerKind::Fabric,
                Loader::Quilt => crate::loader_catalog::InstallerKind::Quilt,
            },
            from_remote,
        );
    }

    fn install_forge_shortcut(&mut self, kind: crate::forge::ForgeKind, from_remote: bool) {
        self.choose_loader(
            match kind {
                crate::forge::ForgeKind::Forge => crate::loader_catalog::InstallerKind::Forge,
                crate::forge::ForgeKind::NeoForge => crate::loader_catalog::InstallerKind::NeoForge,
            },
            from_remote,
        );
    }

    fn trigger_download_action(&mut self, action: usize) {
        if action == 6 {
            if self.task.is_none() {
                self.editor = Some(Editor {
                    kind: EditKind::ImportPack,
                    text: String::new(),
                });
            }
            return;
        }
        if self.remote.is_empty() {
            self.fetch_catalog();
            return;
        }
        match action {
            0 => {
                if let Some(remote) = self.selected_remote() {
                    let config = self.config.clone();
                    self.work(WorkKind::Download, move |b| {
                        b.install(&config, &remote)?;
                        Ok(tformat!("{} 安装完成", remote.id))
                    });
                }
            }
            1 => self.install_loader_shortcut(Loader::Fabric, true),
            2 => self.install_forge_shortcut(crate::forge::ForgeKind::NeoForge, true),
            3 => self.install_forge_shortcut(crate::forge::ForgeKind::Forge, true),
            4 => self.install_loader_shortcut(Loader::Quilt, true),
            5 => self.choose_loader(crate::loader_catalog::InstallerKind::OptiFine, true),
            _ => {}
        }
    }

    fn trigger_account_action(&mut self, action: usize) {
        if self.task.is_some() {
            self.task_popup = true;
            return;
        }
        match action {
            0 => {
                if let Some(account) = self.accounts.entries.get(self.account_sel) {
                    let id = account.id.clone();
                    let result = self.accounts.select(&id);
                    self.report(result.map(|_| tr("账户已切换").into()));
                } else {
                    self.editor = Some(Editor {
                        kind: EditKind::OfflineAccount,
                        text: self.config.username.clone(),
                    });
                }
            }
            1 => {
                let config = self.config.clone();
                self.work(WorkKind::Other, move |b| b.login_microsoft(&config));
                self.show_logs();
            }
            2 => {
                self.editor = Some(Editor {
                    kind: EditKind::OfflineAccount,
                    text: self.config.username.clone(),
                });
            }
            3 => {
                self.editor = Some(Editor {
                    kind: EditKind::CurseKey,
                    text: String::new(),
                });
            }
            4 => {
                if let Some(account) = self.accounts.entries.get(self.account_sel) {
                    let id = account.id.clone();
                    let result = self.accounts.remove(&id);
                    self.report(result.map(|_| "账户与本机凭证已移除".into()));
                    self.account_sel = self
                        .account_sel
                        .min(self.accounts.entries.len().saturating_sub(1));
                }
            }
            _ => {}
        }
    }

    fn search_online_mods(&mut self, query: String, offset: u64) {
        let Some(id) = self.active_instance() else {
            self.log(tr("请先在实例列表选择一个已安装实例"));
            return;
        };
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
    }

    fn install_online_mod(&mut self, project: String, version: Option<String>) {
        if let Some(id) = self.active_instance() {
            let config = self.config.clone();
            self.work(WorkKind::Download, move |b| {
                b.install_mod(&config, &id, &project, version.as_deref())
            });
        }
    }

    fn load_mod_versions(&mut self) {
        if let (Some(id), Some(result)) = (self.active_instance(), &self.mod_search) {
            if let Some(hit) = result.hits.get(self.mod_sel) {
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
        if let Some(id) = self.active_instance() {
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

    fn detect_java(&mut self) {
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

    fn save_editor(&mut self) {
        let Some(editor) = self.editor.take() else {
            return;
        };
        let result = match editor.kind {
            EditKind::InstanceSearch => {
                self.instance_query = editor.text.clone();
                self.instance_sel = 0;
                self.sync_selection_to_managed();
                Ok(())
            }
            EditKind::RemoteSearch => {
                self.remote_query = editor.text.clone();
                self.remote_sel = 0;
                Ok(())
            }
            EditKind::GlobalSetting(idx) => self.save_setting(None, idx, &editor.text),
            EditKind::InstanceSetting(idx) => {
                let scope = self.editor_instance.clone();
                self.save_setting(scope.as_deref(), idx, &editor.text)
            }
            EditKind::BackupWorld => {
                if let (Some(id), Some(world)) = (
                    self.active_instance(),
                    self.world_list.get(self.world_sel).cloned(),
                ) {
                    let config = self.config.clone();
                    let output = PathBuf::from(editor.text.trim().trim_matches('"'));
                    self.work(WorkKind::Other, move |b| {
                        b.backup_world(&config, &id, &world, &output)?;
                        Ok(tformat!("存档已备份：{}", output.display()))
                    });
                }
                Ok(())
            }
            EditKind::ImportWorldZip => {
                self.pending_world_zip = Some(PathBuf::from(editor.text.trim().trim_matches('"')));
                self.editor = Some(Editor {
                    kind: EditKind::ImportWorldName,
                    text: "导入的世界".into(),
                });
                Ok(())
            }
            EditKind::ImportWorldName => {
                if let (Some(id), Some(input)) =
                    (self.active_instance(), self.pending_world_zip.take())
                {
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
                    let path = PathBuf::from(editor.text.trim_matches('"'));
                    self.work(WorkKind::Other, move |b| {
                        b.export_pack(&config, &id, &path)?;
                        Ok(tformat!("导出完成：{}", path.display()))
                    });
                    Ok(())
                } else {
                    Err(anyhow::anyhow!("未选择实例"))
                }
            }
            EditKind::OfflineAccount => self.accounts.add_offline(&editor.text),
            EditKind::ImportPack => {
                let config = self.config.clone();
                let path = PathBuf::from(editor.text.trim_matches('"'));
                self.work(WorkKind::Download, move |b| {
                    let id = b.import_pack(&config, &path, None)?;
                    Ok(tformat!("整合包已导入：{id}", id = id))
                });
                Ok(())
            }
            EditKind::ImportMod => self
                .active_instance()
                .context("请先在实例列表选择一个已安装实例")
                .and_then(|id| {
                    mods::import(&self.config, &id, Path::new(editor.text.trim_matches('"')))
                }),
        };

        if let Err(e) = result {
            self.log(e.to_string());
            self.editor = Some(editor);
        } else {
            match editor.kind {
                EditKind::CurseKey => self.log(tr("CurseForge API 密钥已保存在系统安全存储")),
                EditKind::NameInstance => {
                    self.names = crate::instances::names(&self.config);
                    self.log(tr("显示名称已更新，实例 ID 与依赖关系保持稳定"));
                }
                EditKind::ImportMod => {
                    self.log(tr("Mod 已导入"));
                    self.sync_managed_data();
                }
                EditKind::OfflineAccount => {
                    self.account_sel = self
                        .accounts
                        .active
                        .as_ref()
                        .and_then(|id| self.accounts.entries.iter().position(|a| &a.id == id))
                        .unwrap_or(0);
                    self.log(tr("账户已切换"));
                }
                _ => {}
            }
        }
    }

    // ── Backend Channel Message Pump ───────────────────────────────────────

    pub(super) fn messages(&mut self) {
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
                    if self.active_instance().as_ref() == Some(&instance) {
                        self.task_handoff = true;
                        self.mod_updates_only = updates;
                        self.mod_query = query;
                        self.mod_search = Some(result);
                        self.mod_versions = None;
                        self.mod_sel = 0;
                    }
                }
                Message::ModVersions {
                    instance,
                    project,
                    versions,
                } => {
                    if self.active_instance().as_ref() == Some(&instance) {
                        self.task_handoff = true;
                        self.mod_versions = Some((project, versions));
                        self.mod_sel = 0;
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
                    self.task_handoff = true;
                    self.remote = versions;
                    self.remote_sel = 0;
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
                            Ok(msg) | Err(msg) => msg.clone(),
                        };
                        self.task_result = Some((title.into(), message));
                        self.result_scroll = 0;
                    }
                    self.task = None;
                    self.task_popup = false;
                    self.transfer = None;
                    self.paused = false;
                    self.confirm_stop = false;
                    self.progress = (tr("空闲").into(), 0, 0);
                    self.local = backend::installed(&self.config);
                    self.names = crate::instances::names(&self.config);
                    self.restore_instance_selection();
                    self.sync_managed_data();
                    match Accounts::load() {
                        Ok(accounts) => {
                            self.accounts = accounts;
                            self.account_sel = self
                                .account_sel
                                .min(self.accounts.entries.len().saturating_sub(1));
                        }
                        Err(e) => self.log(e.to_string()),
                    }
                    match result {
                        Ok(s) => self.log(s),
                        Err(e) => self.log(tformat!("错误 · {e}", e = e)),
                    }
                }
            }
        }
    }

    // ── Keyboard Input Router ──────────────────────────────────────────────

    pub(super) fn key(&mut self, key: event::KeyEvent) -> bool {
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

        // 1. Modal priority stack
        if let Some(selected) = self.language_picker {
            let count = crate::localization::UiLanguage::ALL.len() + 1;
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.language_picker = Some((selected + count - 1) % count)
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.language_picker = Some((selected + 1) % count)
                }
                KeyCode::Home => self.language_picker = Some(0),
                KeyCode::End => self.language_picker = Some(count - 1),
                KeyCode::Esc => self.language_picker = None,
                KeyCode::Enter => {
                    let code = if selected == 0 {
                        "auto"
                    } else {
                        crate::localization::UiLanguage::ALL[selected - 1].game_code()
                    };
                    match self.save_setting(None, 16, code) {
                        Ok(()) => self.language_picker = None,
                        Err(error) => self.log(format!("{error:#}")),
                    }
                }
                _ => {}
            }
            return false;
        }

        if let Some(id) = self.confirm_remove.clone() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    let result = crate::instances::trash(&self.config, &id);
                    self.report(result.map(|entry| {
                        tformat!("实例已移至回收站，可在设置 › 回收站恢复：{}", entry.label)
                    }));
                    self.confirm_remove = None;
                    self.local = backend::installed(&self.config);
                    self.names = crate::instances::names(&self.config);
                    self.restore_instance_selection();
                    self.sync_managed_data();
                    self.focus = PaneFocus::Left;
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                    self.confirm_remove = None
                }
                _ => {}
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
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => self.confirm_stop = false,
                _ => {}
            }
            return false;
        }

        if self.help {
            match key.code {
                KeyCode::Esc | KeyCode::F(1) | KeyCode::Char('q') | KeyCode::Char('?') => {
                    self.help = false;
                    self.help_scroll = 0;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.help_scroll = (self.help_scroll + 1).min(24)
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.help_scroll = self.help_scroll.saturating_sub(1)
                }
                KeyCode::Home => self.help_scroll = 0,
                _ => {}
            }
            return false;
        }

        if self.editor.is_some() {
            match key.code {
                KeyCode::Esc => {
                    self.editor = None;
                    self.pending_world_zip = None;
                }
                KeyCode::Enter => self.save_editor(),
                KeyCode::Backspace => {
                    self.editor.as_mut().unwrap().text.pop();
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.editor.as_mut().unwrap().text.clear();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.editor.as_mut().unwrap().text.push(c);
                }
                _ => {}
            }
            return false;
        }

        if self.task_result.is_some() {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.result_scroll = self.result_scroll.saturating_sub(1)
                }
                KeyCode::Down | KeyCode::Char('j') => {
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
                    self.loader_picker = None;
                    self.show_logs();
                }
                _ => {}
            }
            return false;
        }

        if self.task_popup && self.task.is_some() {
            match key.code {
                KeyCode::Esc | KeyCode::F(6) => self.task_popup = false,
                KeyCode::Char('l') => {
                    self.task_popup = false;
                    self.loader_picker = None;
                    self.show_logs();
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
                KeyCode::F(1) | KeyCode::Char('?') => self.help = true,
                _ => {}
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
                _ => {}
            }
            return false;
        }

        // 2. Global keys (work anywhere outside modals)
        match key.code {
            KeyCode::F(1) | KeyCode::Char('?') => {
                self.help = true;
                return false;
            }
            KeyCode::F(6) if self.task.is_some() => {
                self.task_popup = true;
                return false;
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.task.is_none() {
                    return true;
                }
                self.log(tr("请按 C 取消任务后退出"));
                return false;
            }
            KeyCode::Char('q') => {
                if self.task.is_none() {
                    return true;
                }
                self.log(tr("任务正在运行；C 取消任务，游戏终止需再按 Y 确认"));
                return false;
            }
            KeyCode::Char('c') if self.task.is_some() => {
                if self.task == Some(WorkKind::Game) {
                    self.confirm_stop = true;
                } else {
                    self.control.cancel();
                    self.paused = false;
                    self.log(tr("正在取消，等待当前操作结束…"));
                }
                return false;
            }
            KeyCode::Char('p') if self.task.is_some() && self.pause_allowed => {
                if !self.control.cancelled() {
                    self.paused = self.control.toggle_pause();
                }
                return false;
            }
            KeyCode::Char(c @ '1'..='5') => {
                let idx = (c as usize) - ('1' as usize);
                self.switch_page(PrimaryPage::ALL[idx]);
                return false;
            }
            KeyCode::Tab | KeyCode::Char(']') => {
                let next = (self.page.index() + 1) % PrimaryPage::ALL.len();
                self.switch_page(PrimaryPage::ALL[next]);
                return false;
            }
            KeyCode::BackTab | KeyCode::Char('[') => {
                let prev =
                    (self.page.index() + PrimaryPage::ALL.len() - 1) % PrimaryPage::ALL.len();
                self.switch_page(PrimaryPage::ALL[prev]);
                return false;
            }
            KeyCode::Char('r') => {
                self.refresh();
                return false;
            }
            KeyCode::Char('v')
                if !(self.page == PrimaryPage::Play
                    && self.focus == PaneFocus::Right
                    && self.wb_tab == WorkbenchTab::Mods
                    && self.mod_search.is_some()) =>
            {
                self.detect_java();
                return false;
            }
            _ => {}
        }

        // 3. Page-specific input routing
        match self.page {
            PrimaryPage::Play => self.key_play(key),
            PrimaryPage::Downloads => self.key_downloads(key),
            PrimaryPage::Accounts => self.key_accounts(key),
            PrimaryPage::Settings => self.key_settings(key),
            PrimaryPage::Logs => self.key_logs(key),
        }
        false
    }

    fn key_play(&mut self, key: event::KeyEvent) {
        if self.focus == PaneFocus::Left {
            let count = self.local_visible().len();
            if move_cursor(&mut self.instance_sel, count, key.code) {
                self.sync_selection_to_managed();
                return;
            }
            match key.code {
                KeyCode::Char(' ') => {
                    if let Some(id) = self.selected_instance() {
                        self.launch_instance(id);
                    }
                }
                KeyCode::Enter | KeyCode::Right => {
                    if self.selected_instance().is_some() {
                        self.sync_selection_to_managed();
                        self.focus = PaneFocus::Right;
                    }
                }
                KeyCode::Char('/') => {
                    self.editor = Some(Editor {
                        kind: EditKind::InstanceSearch,
                        text: self.instance_query.clone(),
                    });
                }
                KeyCode::Char('o') => {
                    if self.selected_instance().is_some() {
                        self.sync_selection_to_managed();
                        self.wb_tab = WorkbenchTab::Settings;
                        self.focus = PaneFocus::Right;
                    }
                }
                KeyCode::Char('m') => {
                    if self.selected_instance().is_some() {
                        self.sync_selection_to_managed();
                        self.wb_tab = WorkbenchTab::Mods;
                        self.focus = PaneFocus::Right;
                    }
                }
                KeyCode::Char('w') => {
                    if self.selected_instance().is_some() {
                        self.sync_selection_to_managed();
                        self.wb_tab = WorkbenchTab::Worlds;
                        self.focus = PaneFocus::Right;
                    }
                }
                KeyCode::Char('i') if self.task.is_none() => {
                    self.editor = Some(Editor {
                        kind: EditKind::ImportPack,
                        text: String::new(),
                    });
                }
                KeyCode::Char('x') => self.trigger_overview_action(1),
                KeyCode::Char('b') => self.trigger_overview_action(2),
                KeyCode::Char('d') => self.trigger_overview_action(3),
                KeyCode::Char('l') => self.trigger_overview_action(4),
                KeyCode::F(2) => self.trigger_overview_action(5),
                KeyCode::Char('t') => self.trigger_overview_action(6),
                KeyCode::Char('e') => self.trigger_overview_action(7),
                KeyCode::Delete => self.trigger_overview_action(8),
                KeyCode::Char('f') => self.install_loader_shortcut(Loader::Fabric, false),
                KeyCode::Char('u') => self.install_loader_shortcut(Loader::Quilt, false),
                KeyCode::Char('h') => {
                    self.install_forge_shortcut(crate::forge::ForgeKind::Forge, false)
                }
                KeyCode::Char('n') => {
                    self.install_forge_shortcut(crate::forge::ForgeKind::NeoForge, false)
                }
                KeyCode::F(7) => {
                    self.choose_loader(crate::loader_catalog::InstallerKind::OptiFine, false)
                }
                _ => {}
            }
            return;
        }

        // Right Pane: Instance Workbench
        match key.code {
            KeyCode::Right | KeyCode::Char('l') if !(self.wb_tab == WorkbenchTab::Mods) => {
                self.wb_tab = self.wb_tab.next();
                return;
            }
            KeyCode::Right if self.wb_tab == WorkbenchTab::Mods => {
                self.wb_tab = self.wb_tab.next();
                return;
            }
            KeyCode::Left => {
                if self.wb_tab == WorkbenchTab::Overview {
                    self.focus = PaneFocus::Left;
                } else {
                    self.wb_tab = self.wb_tab.prev();
                }
                return;
            }
            KeyCode::Esc => {
                if self.wb_tab == WorkbenchTab::Mods && self.mod_versions.is_some() {
                    self.mod_versions = None;
                    self.mod_sel = 0;
                } else if self.wb_tab == WorkbenchTab::Mods && self.mod_search.is_some() {
                    self.mod_search = None;
                    self.mod_sel = 0;
                } else {
                    self.focus = PaneFocus::Left;
                }
                return;
            }
            KeyCode::Backspace if self.wb_tab != WorkbenchTab::Mods => {
                self.focus = PaneFocus::Left;
                return;
            }
            _ => {}
        }

        match self.wb_tab {
            WorkbenchTab::Overview => {
                if move_cursor(&mut self.wb_overview_sel, OVERVIEW_ACTIONS.len(), key.code) {
                    return;
                }
                match key.code {
                    KeyCode::Enter => self.trigger_overview_action(self.wb_overview_sel),
                    KeyCode::Char(' ') => self.trigger_overview_action(0),
                    _ => {}
                }
            }
            WorkbenchTab::Settings => {
                if move_cursor(
                    &mut self.wb_setting_sel,
                    INSTANCE_SETTING_INDICES.len(),
                    key.code,
                ) {
                    return;
                }
                let idx = self.active_instance_setting_index();
                match key.code {
                    KeyCode::Enter => {
                        if let Some(id) = self.active_instance() {
                            self.edit_or_toggle_setting(Some(id), idx, false);
                        }
                    }
                    KeyCode::Char(' ') => {
                        if let Some(id) = self.active_instance() {
                            self.edit_or_toggle_setting(Some(id), idx, true);
                        }
                    }
                    KeyCode::Char('d') if self.task.is_none() => {
                        if let Some(id) = self.active_instance() {
                            let res = crate::instances::reset_settings(&mut self.config, &id);
                            self.report(res.map(|_| "实例已恢复继承全局设置".into()));
                        }
                    }
                    KeyCode::Char('g') => self.switch_page(PrimaryPage::Settings),
                    _ => {}
                }
            }
            WorkbenchTab::Mods => {
                let count = self.mod_item_count();
                if move_cursor(&mut self.mod_sel, count, key.code) {
                    return;
                }
                match key.code {
                    KeyCode::Enter | KeyCode::Char(' ') if self.task.is_none() => {
                        if let Some((project, versions)) = &self.mod_versions {
                            if let Some(ver) = versions.get(self.mod_sel) {
                                self.install_online_mod(project.clone(), Some(ver.id.clone()));
                            }
                        } else if let Some(search) = &self.mod_search {
                            if let Some(hit) = search.hits.get(self.mod_sel) {
                                self.install_online_mod(hit.project_id.clone(), None);
                            }
                        } else if let (Some(id), Some(entry)) =
                            (self.active_instance(), self.mod_list.get(self.mod_sel))
                        {
                            let res = mods::toggle(&self.config, &id, &entry.filename);
                            self.report(res.map(|_| "Mod 状态已更新，下次启动生效".into()));
                            self.sync_managed_data();
                        }
                    }
                    KeyCode::Char('a') if self.task.is_none() => {
                        self.editor = Some(Editor {
                            kind: EditKind::ImportMod,
                            text: String::new(),
                        });
                    }
                    KeyCode::Char('b' | '/') if self.task.is_none() => {
                        self.editor = Some(Editor {
                            kind: EditKind::ModQuery,
                            text: self.mod_query.clone(),
                        });
                    }
                    KeyCode::Char('v') if self.mod_search.is_some() && self.task.is_none() => {
                        self.load_mod_versions();
                    }
                    KeyCode::Char('u') if self.task.is_none() => self.check_mod_updates(),
                    KeyCode::Char('l') => {
                        self.mod_search = None;
                        self.mod_versions = None;
                        self.mod_sel = 0;
                        self.sync_managed_data();
                    }
                    KeyCode::Backspace => {
                        if self.mod_versions.is_some() {
                            self.mod_versions = None;
                            self.mod_sel = 0;
                        } else if self.mod_search.is_some() {
                            self.mod_search = None;
                            self.mod_sel = 0;
                        } else {
                            self.focus = PaneFocus::Left;
                        }
                    }
                    KeyCode::Char('n') if self.task.is_none() && !self.mod_updates_only => {
                        if let Some(res) = &self.mod_search {
                            let offset = res.offset.saturating_add(20);
                            if offset < res.total_hits {
                                self.search_online_mods(self.mod_query.clone(), offset);
                            }
                        }
                    }
                    KeyCode::Char('p') if self.task.is_none() && !self.mod_updates_only => {
                        if let Some(res) = &self.mod_search {
                            self.search_online_mods(
                                self.mod_query.clone(),
                                res.offset.saturating_sub(20),
                            );
                        }
                    }
                    KeyCode::Delete
                        if self.task.is_none()
                            && self.mod_search.is_none()
                            && self.mod_versions.is_none() =>
                    {
                        if let (Some(id), Some(entry)) =
                            (self.active_instance(), self.mod_list.get(self.mod_sel))
                        {
                            let res = mods::remove(&self.config, &id, &entry.filename)
                                .map(|backup| tformat!("Mod 已移出，备份：{}", backup.display()));
                            self.report(res);
                            self.sync_managed_data();
                        }
                    }
                    KeyCode::Char('z')
                        if self.task.is_none()
                            && self.mod_search.is_none()
                            && self.mod_versions.is_none() =>
                    {
                        if let Some(id) = self.active_instance() {
                            let res = mods::removed(&self.config, &id).and_then(|entries| {
                                let entry = entries.first().context("没有可恢复的 Mod 备份")?;
                                mods::restore(&self.config, &id, &entry.token)
                            });
                            self.report(res.map(|filename| {
                                tformat!("已恢复 {filename}", filename = filename)
                            }));
                            self.sync_managed_data();
                        }
                    }
                    _ => {}
                }
            }
            WorkbenchTab::Worlds => {
                if move_cursor(&mut self.world_sel, self.world_list.len(), key.code) {
                    return;
                }
                match key.code {
                    KeyCode::Enter | KeyCode::Char('b') if self.task.is_none() => {
                        if let Some(world) = self.world_list.get(self.world_sel) {
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
                    _ => {}
                }
            }
            WorkbenchTab::Loaders => {
                if move_cursor(&mut self.wb_loader_sel, LOADER_ITEMS.len(), key.code) {
                    return;
                }
                if key.code == KeyCode::Enter {
                    let kind = match self.wb_loader_sel {
                        0 => crate::loader_catalog::InstallerKind::Fabric,
                        1 => crate::loader_catalog::InstallerKind::Quilt,
                        2 => crate::loader_catalog::InstallerKind::Forge,
                        3 => crate::loader_catalog::InstallerKind::NeoForge,
                        _ => crate::loader_catalog::InstallerKind::OptiFine,
                    };
                    self.choose_loader(kind, false);
                }
            }
        }
    }

    fn key_downloads(&mut self, key: event::KeyEvent) {
        if self.focus == PaneFocus::Left {
            let count = self.remote_visible().len();
            if move_cursor(&mut self.remote_sel, count, key.code) {
                return;
            }
            match key.code {
                KeyCode::Enter => self.trigger_download_action(0),
                KeyCode::Right => self.focus = PaneFocus::Right,
                KeyCode::Char('s') => {
                    self.snapshots = !self.snapshots;
                    self.remote_sel = 0;
                }
                KeyCode::Char('/') => {
                    self.editor = Some(Editor {
                        kind: EditKind::RemoteSearch,
                        text: self.remote_query.clone(),
                    });
                }
                KeyCode::Char('f') => self.trigger_download_action(1),
                KeyCode::Char('n') => self.trigger_download_action(2),
                KeyCode::Char('h') => self.trigger_download_action(3),
                KeyCode::Char('u') => self.trigger_download_action(4),
                KeyCode::Char('o') | KeyCode::F(7) => self.trigger_download_action(5),
                KeyCode::Char('i') => self.trigger_download_action(6),
                _ => {}
            }
        } else {
            if move_cursor(
                &mut self.download_action_sel,
                DOWNLOAD_ACTIONS.len(),
                key.code,
            ) {
                return;
            }
            match key.code {
                KeyCode::Enter => self.trigger_download_action(self.download_action_sel),
                KeyCode::Left | KeyCode::Esc | KeyCode::Backspace => self.focus = PaneFocus::Left,
                KeyCode::Char('s') => {
                    self.snapshots = !self.snapshots;
                    self.remote_sel = 0;
                }
                _ => {}
            }
        }
    }

    fn key_accounts(&mut self, key: event::KeyEvent) {
        if self.focus == PaneFocus::Left {
            let count = self.accounts.entries.len();
            if move_cursor(&mut self.account_sel, count, key.code) {
                return;
            }
            match key.code {
                KeyCode::Enter => self.trigger_account_action(0),
                KeyCode::Right => self.focus = PaneFocus::Right,
                KeyCode::Char('a') => self.trigger_account_action(1),
                KeyCode::Char('n') => self.trigger_account_action(2),
                KeyCode::Char('K') => self.trigger_account_action(3),
                KeyCode::Char('d') | KeyCode::Delete => self.trigger_account_action(4),
                _ => {}
            }
        } else {
            if move_cursor(
                &mut self.account_action_sel,
                ACCOUNT_ACTIONS.len(),
                key.code,
            ) {
                return;
            }
            match key.code {
                KeyCode::Enter => self.trigger_account_action(self.account_action_sel),
                KeyCode::Left | KeyCode::Esc | KeyCode::Backspace => self.focus = PaneFocus::Left,
                KeyCode::Char('a') => self.trigger_account_action(1),
                KeyCode::Char('n') => self.trigger_account_action(2),
                KeyCode::Char('K') => self.trigger_account_action(3),
                KeyCode::Char('d') | KeyCode::Delete => self.trigger_account_action(4),
                _ => {}
            }
        }
    }

    fn key_settings(&mut self, key: event::KeyEvent) {
        if self.focus == PaneFocus::Left {
            if move_cursor(&mut self.settings_cat, SETTINGS_CATEGORIES.len(), key.code) {
                self.settings_field_sel = 0;
                if self.settings_cat == 5 {
                    self.refresh_trash();
                }
                return;
            }
            if matches!(key.code, KeyCode::Enter | KeyCode::Right) {
                if self.settings_cat == 5 {
                    self.refresh_trash();
                }
                self.focus = PaneFocus::Right;
            }
        } else {
            if matches!(key.code, KeyCode::Left | KeyCode::Esc | KeyCode::Backspace) {
                self.focus = PaneFocus::Left;
                return;
            }
            if self.settings_cat == 5 {
                if move_cursor(&mut self.trash_sel, self.trash.len(), key.code) {
                    return;
                }
                if key.code == KeyCode::Enter && self.task.is_none() {
                    if let Some(entry) = self.trash.get(self.trash_sel) {
                        let token = entry.token.clone();
                        let res = crate::instances::restore(&self.config, &token);
                        self.report(res.map(|restored| {
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
            } else {
                let count = self.global_setting_indices().len();
                if move_cursor(&mut self.settings_field_sel, count, key.code) {
                    return;
                }
                if let Some(idx) = self.active_global_setting_index() {
                    match key.code {
                        KeyCode::Enter => self.edit_or_toggle_setting(None, idx, false),
                        KeyCode::Char(' ') => self.edit_or_toggle_setting(None, idx, true),
                        _ => {}
                    }
                }
            }
        }
    }

    fn key_logs(&mut self, key: event::KeyEvent) {
        let view_h = self.log_view_height.get().max(1);
        let max_scroll = self.logs.len().saturating_sub(view_h);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.log_scroll = self.log_scroll.saturating_add(1).min(max_scroll);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.log_scroll = self.log_scroll.saturating_sub(1);
            }
            KeyCode::PageUp => {
                self.log_scroll = self.log_scroll.saturating_add(view_h).min(max_scroll);
            }
            KeyCode::PageDown => {
                self.log_scroll = self.log_scroll.saturating_sub(view_h);
            }
            KeyCode::Home => self.log_scroll = max_scroll,
            KeyCode::End => self.log_scroll = 0,
            KeyCode::Right => self.log_horizontal = self.log_horizontal.saturating_add(8),
            KeyCode::Left => self.log_horizontal = self.log_horizontal.saturating_sub(8),
            KeyCode::Esc | KeyCode::Backspace => {
                if let Some((prev_page, prev_focus)) = self.return_page.take() {
                    self.page = prev_page;
                    self.focus = prev_focus;
                }
            }
            _ => {}
        }
    }

    // ── Mouse Interaction ──────────────────────────────────────────────────

    fn has_active_modal(&self) -> bool {
        self.language_picker.is_some()
            || self.confirm_remove.is_some()
            || self.confirm_stop
            || self.help
            || self.editor.is_some()
            || self.task_result.is_some()
            || (self.task_popup && self.task.is_some())
            || self.loader_picker.is_some()
    }

    pub(super) fn mouse(&mut self, event: MouseEvent) {
        if self.has_active_modal() {
            match event.kind {
                MouseEventKind::ScrollUp => {
                    let _ = self.key(event::KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
                }
                MouseEventKind::ScrollDown => {
                    let _ = self.key(event::KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
                }
                _ => {}
            }
            return;
        }

        let col = event.column;
        let row = event.row;
        let hits = self.hits.borrow().clone();

        match event.kind {
            MouseEventKind::ScrollUp => {
                for (rect, target) in hits.iter().rev() {
                    if contains(*rect, col, row) {
                        if let HitTarget::FocusPane(pane) | HitTarget::ListArea { pane, .. } =
                            target
                        {
                            self.focus = *pane;
                            break;
                        }
                    }
                }
                let _ = self.key(event::KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
            }
            MouseEventKind::ScrollDown => {
                for (rect, target) in hits.iter().rev() {
                    if contains(*rect, col, row) {
                        if let HitTarget::FocusPane(pane) | HitTarget::ListArea { pane, .. } =
                            target
                        {
                            self.focus = *pane;
                            break;
                        }
                    }
                }
                let _ = self.key(event::KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
            }
            MouseEventKind::Down(MouseButton::Left) => {
                for (rect, target) in hits.iter().rev() {
                    if !contains(*rect, col, row) {
                        continue;
                    }
                    match *target {
                        HitTarget::Page(page) => {
                            self.switch_page(page);
                            return;
                        }
                        HitTarget::WorkbenchTab(tab) => {
                            self.wb_tab = tab;
                            self.focus = PaneFocus::Right;
                            return;
                        }
                        HitTarget::LaunchNow => {
                            if let Some(id) = self.active_instance() {
                                self.launch_instance(id);
                            }
                            return;
                        }
                        HitTarget::TaskTogglePopup => {
                            if self.task.is_some() {
                                self.task_popup = !self.task_popup;
                            }
                            return;
                        }
                        HitTarget::ListArea {
                            pane,
                            offset,
                            row_height,
                            count,
                        } => {
                            if count == 0 || row_height == 0 {
                                self.focus = pane;
                                return;
                            }
                            let clicked_idx = offset + ((row - rect.y) / row_height) as usize;
                            if clicked_idx < count {
                                let already_focused = self.focus == pane;
                                self.focus = pane;
                                let already_selected = self.select_in_pane(pane, clicked_idx);
                                if already_focused && already_selected {
                                    let _ = self.key(event::KeyEvent::new(
                                        KeyCode::Enter,
                                        KeyModifiers::NONE,
                                    ));
                                }
                            } else {
                                self.focus = pane;
                            }
                            return;
                        }
                        HitTarget::FocusPane(pane) => {
                            self.focus = pane;
                            return;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Updates cursor in `pane` to `idx`; returns `true` if `idx` was already selected.
    fn select_in_pane(&mut self, pane: PaneFocus, idx: usize) -> bool {
        match (self.page, pane) {
            (PrimaryPage::Play, PaneFocus::Left) => {
                let same = self.instance_sel == idx;
                self.instance_sel = idx;
                self.sync_selection_to_managed();
                same
            }
            (PrimaryPage::Play, PaneFocus::Right) => match self.wb_tab {
                WorkbenchTab::Overview => replace_eq(&mut self.wb_overview_sel, idx),
                WorkbenchTab::Settings => replace_eq(&mut self.wb_setting_sel, idx),
                WorkbenchTab::Mods => replace_eq(&mut self.mod_sel, idx),
                WorkbenchTab::Worlds => replace_eq(&mut self.world_sel, idx),
                WorkbenchTab::Loaders => replace_eq(&mut self.wb_loader_sel, idx),
            },
            (PrimaryPage::Downloads, PaneFocus::Left) => replace_eq(&mut self.remote_sel, idx),
            (PrimaryPage::Downloads, PaneFocus::Right) => {
                replace_eq(&mut self.download_action_sel, idx)
            }
            (PrimaryPage::Accounts, PaneFocus::Left) => replace_eq(&mut self.account_sel, idx),
            (PrimaryPage::Accounts, PaneFocus::Right) => {
                replace_eq(&mut self.account_action_sel, idx)
            }
            (PrimaryPage::Settings, PaneFocus::Left) => {
                let same = self.settings_cat == idx;
                if !same {
                    self.settings_cat = idx;
                    self.settings_field_sel = 0;
                    if idx == 5 {
                        self.refresh_trash();
                    }
                }
                same
            }
            (PrimaryPage::Settings, PaneFocus::Right) => {
                if self.settings_cat == 5 {
                    replace_eq(&mut self.trash_sel, idx)
                } else {
                    replace_eq(&mut self.settings_field_sel, idx)
                }
            }
            (PrimaryPage::Logs, _) => false,
        }
    }
}

fn replace_eq(slot: &mut usize, next: usize) -> bool {
    let same = *slot == next;
    *slot = next;
    same
}

fn contains(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x
        && col < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

fn move_cursor(cursor: &mut usize, count: usize, code: KeyCode) -> bool {
    if count == 0 {
        *cursor = 0;
        return matches!(
            code,
            KeyCode::Up
                | KeyCode::Down
                | KeyCode::Char('j')
                | KeyCode::Char('k')
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::PageUp
                | KeyCode::PageDown
        );
    }
    let last = count - 1;
    match code {
        KeyCode::Down | KeyCode::Char('j') => *cursor = (*cursor + 1) % count,
        KeyCode::Up | KeyCode::Char('k') => *cursor = (*cursor + count - 1) % count,
        KeyCode::Home => *cursor = 0,
        KeyCode::End => *cursor = last,
        KeyCode::PageUp => *cursor = cursor.saturating_sub(10),
        KeyCode::PageDown => *cursor = (*cursor + 10).min(last),
        _ => return false,
    }
    true
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
        let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
    }
}

pub fn run(config: Config) -> Result<()> {
    crate::localization::set(&config.language);
    enable_raw_mode()?;
    let _guard = TerminalGuard;
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let (tx, rx) = mpsc::channel();
    let mut app = App::new(config, tx, rx);
    app.load_cached_catalog();
    app.log(tr(
        "欢迎使用 Tui-Launcher。Space 立即启动 · Enter 进入实例工作台 · 1–5 切换页面 · 支持鼠标点击与滚轮",
    ));
    loop {
        app.messages();
        terminal.draw(|f| draw(f, &app))?;
        if event::poll(Duration::from_millis(60))? {
            match event::read()? {
                Event::Key(key) => {
                    if app.key(key) {
                        break;
                    }
                }
                Event::Mouse(mouse) => app.mouse(mouse),
                _ => {}
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use theme::input_tail;

    fn test_app() -> App {
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(
            Config {
                language: "zh_cn".into(),
                ..Config::default()
            },
            tx,
            rx,
        );
        app.local = vec!["1.21.1".into(), "fabric-loader-1.20.1".into()];
        app.managed_instance = Some("1.21.1".into());
        app
    }

    #[test]
    fn all_workspaces_and_workbench_tabs_render_cleanly() {
        let mut app = test_app();
        app.names
            .insert("fabric-loader-1.20.1".into(), "生存整合".into());
        app.switch_page(PrimaryPage::Play);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(140, 34)).unwrap();
        for page in PrimaryPage::ALL {
            app.switch_page(page);
            let frame = terminal.draw(|f| draw(f, &app)).unwrap();
            let text: String = frame.buffer.content.iter().map(|c| c.symbol()).collect();
            assert!(text.replace(' ', "").contains(page.label()));
        }

        app.switch_page(PrimaryPage::Play);
        app.focus = PaneFocus::Right;
        for tab in WorkbenchTab::ALL {
            app.wb_tab = tab;
            let frame = terminal.draw(|f| draw(f, &app)).unwrap();
            let text: String = frame.buffer.content.iter().map(|c| c.symbol()).collect();
            assert!(text.replace(' ', "").contains(tab.label()));
        }
    }

    #[test]
    fn dual_pane_navigation_and_filtered_instance_identity() {
        let mut app = test_app();
        assert_eq!(app.page, PrimaryPage::Play);
        assert_eq!(app.focus, PaneFocus::Left);

        // Filter instances to fabric and enter workbench
        app.instance_query = "fabric".into();
        app.sync_selection_to_managed();
        assert_eq!(
            app.active_instance().as_deref(),
            Some("fabric-loader-1.20.1")
        );

        app.key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.focus, PaneFocus::Right);
        assert_eq!(app.wb_tab, WorkbenchTab::Overview);

        // Right arrow moves across workbench tabs; Esc returns to left list
        app.key(event::KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(app.wb_tab, WorkbenchTab::Settings);
        app.key(event::KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(app.wb_tab, WorkbenchTab::Mods);
        app.key(event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.focus, PaneFocus::Left);
        assert_eq!(app.instance_query, "fabric");
        assert_eq!(
            app.selected_instance().as_deref(),
            Some("fabric-loader-1.20.1")
        );
    }

    #[test]
    fn settings_categories_cover_every_global_setting_once() {
        let mut covered = Vec::new();
        for group in SETTING_GROUPS {
            covered.extend_from_slice(group);
        }
        covered.sort_unstable();
        assert_eq!(covered, (0..SETTINGS.len()).collect::<Vec<_>>());

        let mut app = test_app();
        app.switch_page(PrimaryPage::Settings);
        assert_eq!(app.focus, PaneFocus::Left);
        app.key(event::KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.settings_cat, 1);
        app.key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.focus, PaneFocus::Right);
        app.key(event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.focus, PaneFocus::Left);
    }

    #[test]
    fn multilingual_picker_and_english_rendering() {
        let mut app = test_app();
        app.config.language = "auto".into();
        app.switch_page(PrimaryPage::Settings);
        app.settings_cat = 3; // General & Language (index 0 is UI Language = 16)
        app.focus = PaneFocus::Right;
        app.key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.language_picker, Some(0));
        app.key(event::KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(app.language_picker, Some(10));

        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(128, 36)).unwrap();
        for language in crate::localization::UiLanguage::ALL {
            app.config.language = language.game_code().into();
            terminal.draw(|f| draw(f, &app)).unwrap();
        }
        app.key(event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.language_picker.is_none());

        app.config.language = "en_us".into();
        app.names.insert("1.21.1".into(), "我的存档服".into());
        app.switch_page(PrimaryPage::Play);
        let rendered = terminal.draw(|f| draw(f, &app)).unwrap();
        let text: String = rendered.buffer.content.iter().map(|c| c.symbol()).collect();
        assert!(text.replace(' ', "").contains("我的存档服"));
        assert!(!text.contains("选择实例按 Space 立即启动"));
    }

    #[test]
    fn responsive_sizes_modals_and_unicode_input_tail() {
        assert_eq!(input_tail("abc中文xyz", 7), "中文xyz");
        assert_eq!(input_tail("中文", 1), "");

        let mut app = test_app();
        for (w, h) in [(20, 8), (48, 16), (72, 20), (100, 28), (150, 42)] {
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
            for page in PrimaryPage::ALL {
                app.switch_page(page);
                terminal.draw(|f| draw(f, &app)).unwrap();
                app.focus = PaneFocus::Right;
                terminal.draw(|f| draw(f, &app)).unwrap();
            }
            app.editor = Some(Editor {
                kind: EditKind::ImportPack,
                text: "长路径/".repeat(40) + "终点.mrpack",
            });
            let frame = terminal.draw(|f| draw(f, &app)).unwrap();
            if w >= 48 && h >= 16 {
                let text: String = frame.buffer.content.iter().map(|c| c.symbol()).collect();
                assert!(text.replace(' ', "").contains("终点.mrpack"));
            }
            app.editor = None;
            app.help = true;
            terminal.draw(|f| draw(f, &app)).unwrap();
            app.help = false;
        }
    }

    #[test]
    fn task_lifecycle_logs_return_and_game_stop_confirmation() {
        let mut app = test_app();
        app.switch_page(PrimaryPage::Downloads);
        app.task = Some(WorkKind::Download);
        app.pause_allowed = true;
        app.task_popup = true;
        app.progress = ("下载资源".into(), 4, 10);

        app.key(event::KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE));
        assert!(app.paused);
        app.key(event::KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
        assert_eq!(app.page, PrimaryPage::Logs);
        app.key(event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.page, PrimaryPage::Downloads);

        app.tx
            .send(Message::Done(Err("校验失败\nEND-MARKER".into())))
            .unwrap();
        app.messages();
        assert!(app.task.is_none());
        assert!(app.task_result.is_some());

        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        app.key(event::KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        terminal.draw(|f| draw(f, &app)).unwrap();
        app.key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.task_result.is_none());

        // Game stop requires Y confirmation and ignores key repeat
        app.task = Some(WorkKind::Game);
        app.key(event::KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        assert!(app.confirm_stop);
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
    fn log_scroll_anchoring_and_mouse_click_routing() {
        let mut app = test_app();
        app.switch_page(PrimaryPage::Logs);
        for i in 0..30 {
            app.log(format!("line {i}"));
        }
        app.log_view_height.set(8);
        app.key(event::KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(app.log_scroll, 8);
        let anchor = app.logs[app.logs.len() - app.log_scroll - 1].clone();
        app.log("incoming");
        assert_eq!(app.logs[app.logs.len() - app.log_scroll - 1], anchor);

        // Render Play page and click top Downloads tab via mouse
        app.switch_page(PrimaryPage::Play);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 32)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let dl_rect = app
            .hits
            .borrow()
            .iter()
            .find_map(|(r, t)| (*t == HitTarget::Page(PrimaryPage::Downloads)).then_some(*r))
            .expect("Downloads tab hit region");
        app.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: dl_rect.x + 1,
            row: dl_rect.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.page, PrimaryPage::Downloads);
    }

    #[test]
    #[ignore]
    fn preview_frames() {
        let mut app = test_app();
        app.config.last_version = Some("fabric-loader-1.20.1".into());
        app.instance_sel = 1;
        app.managed_instance = Some("fabric-loader-1.20.1".into());
        app.names
            .insert("fabric-loader-1.20.1".into(), "生存模组服".into());
        app.remote = vec![
            RemoteVersion {
                id: "1.21.1".into(),
                kind: "release".into(),
                url: String::new(),
                sha1: String::new(),
            },
            RemoteVersion {
                id: "1.20.4".into(),
                kind: "release".into(),
                url: String::new(),
                sha1: String::new(),
            },
            RemoteVersion {
                id: "24w40a".into(),
                kind: "snapshot".into(),
                url: String::new(),
                sha1: String::new(),
            },
        ];
        app.snapshots = true;
        app.log("欢迎使用 Tui-Launcher · 选择实例按 Space 立即启动");

        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(124, 34)).unwrap();
        for (label, page, focus, wb) in [
            (
                "PLAY (LEFT LIST)",
                PrimaryPage::Play,
                PaneFocus::Left,
                WorkbenchTab::Overview,
            ),
            (
                "PLAY (WORKBENCH SETTINGS)",
                PrimaryPage::Play,
                PaneFocus::Right,
                WorkbenchTab::Settings,
            ),
            (
                "DOWNLOADS",
                PrimaryPage::Downloads,
                PaneFocus::Left,
                WorkbenchTab::Overview,
            ),
            (
                "SETTINGS (MASTER-DETAIL)",
                PrimaryPage::Settings,
                PaneFocus::Right,
                WorkbenchTab::Overview,
            ),
        ] {
            app.switch_page(page);
            app.focus = focus;
            app.wb_tab = wb;
            terminal.draw(|f| draw(f, &app)).unwrap();
            let buffer = terminal.backend().buffer().clone();
            let mut out = format!("=== {label} ===\n");
            for y in 0..buffer.area.height {
                let mut skip = 0;
                for x in 0..buffer.area.width {
                    if skip > 0 {
                        skip -= 1;
                        continue;
                    }
                    let symbol = buffer[(x, y)].symbol();
                    skip = ratatui::text::Line::from(symbol).width().saturating_sub(1);
                    out.push_str(symbol);
                }
                out.push('\n');
            }
            println!("{out}");
        }
    }
}

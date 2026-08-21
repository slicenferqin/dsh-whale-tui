//! App state machine: run state, focus, Esc semantics (docs/01 section 2.5),
//! follow-up queue, selection/scroll, and the blocking dialogs (approval
//! prompt + ask_user_question card, docs/01 section 2.4).

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::Sender;
use std::time::Instant;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde_json::{json, Value};

use crate::bus::{AppEvent, Cmd};
use crate::clipboard;
use crate::files::{fuzzy_filter, list_files};
use crate::resume::{list_sessions, read_session_events, SessionSummary};
use crate::term::TermKind;
use crate::theme::Theme;
use crate::transcript::{content_text, CellKind, Transcript};

pub const DOUBLE_ESC_MS: u128 = 800;

/// Transient notices ("copied", "mode: Plan", …) fade after this long; without
/// a TTL they would pin the activity line for the rest of the session.
pub const NOTICE_TTL_MS: u128 = 5000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Idle,
    Starting,
    Running,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Scrollback,
    Prompt,
}

#[derive(Debug, Clone, Copy)]
pub enum EscArm {
    None,
    ClearArmed(Instant),
    RewindArmed(Instant),
}

/// Destructive globals Grok gates behind a double-press within 1s: quitting
/// (throws away the draft) and starting a new session (throws away context).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirm {
    Quit,
    NewSession,
}

pub const CONFIRM_MS: u128 = 1000;

/// Permission prompt (docs/01 section 3.3). Options come from the bridge:
/// allowed-once / rejected (+ always-* rows later).
#[derive(Debug, Clone)]
pub struct ApprovalDialog {
    pub request_id: String,
    pub tool_name: String,
    pub reason: String,
    pub input: String,
    pub call_id: Option<String>,
    pub options: Vec<String>,
    pub selected: usize,
    /// docs/01 section 2.4: Esc parks the keyboard in the scrollback while
    /// the card stays visible; Tab hands the keyboard back to the card.
    pub parked: bool,
}

#[derive(Debug, Clone)]
pub struct Question {
    pub id: String,
    pub question: String,
    pub header: String,
    pub detail: String,
    pub plan_approve: Option<String>,
    pub options: Vec<String>,
    pub multi_select: bool,
}

/// ask_user_question card (docs/01 section 2.4).
#[derive(Debug, Clone)]
pub struct AskDialog {
    pub request_id: String,
    pub questions: Vec<Question>,
    pub current: usize,
    /// Chosen option indices per question.
    pub answers: Vec<Vec<usize>>,
    /// Keyboard cursor per question, independent from the committed selection.
    pub cursors: Vec<usize>,
    /// plan-review: free-form feedback typed after pressing s.
    pub feedback: String,
    pub taking_feedback: bool,
    pub detail_scroll: usize,
    pub custom_text: String,
    pub taking_text: bool,
    pub parked: bool,
}

impl AskDialog {
    fn has_pending_input(&self) -> bool {
        self.taking_text
            || self.taking_feedback
            || !self.custom_text.is_empty()
            || !self.feedback.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct ResumePicker {
    pub items: Vec<SessionSummary>,
    pub selected: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueItem {
    pub id: String,
    pub placement: String,
    pub preview: String,
    pub text: Option<String>,
}

#[derive(Debug, Clone)]
pub struct QueueView {
    pub selected: usize,
    pub editing: bool,
    pub draft: String,
}

#[derive(Debug, Clone)]
pub struct HistoryView {
    pub query: String,
    pub selected: usize,
    pub visible: Vec<usize>,
}

#[derive(Debug, Clone)]
pub struct TaskRow {
    pub id: String,
    pub kind: String,
    pub label: String,
    pub status: String,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct TasksView {
    pub rows: Vec<TaskRow>,
    pub selected: usize,
}

/// Ctrl+T pane over the latest task-list snapshot (docs/01 section 10).
#[derive(Debug, Clone)]
pub struct TodosView {
    pub selected: usize,
}

#[derive(Debug, Clone)]
pub struct SubagentView {
    pub child_id: String,
    pub scroll: usize,
}

#[derive(Debug, Clone)]
pub struct InfoDialog {
    pub rows: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct BlockView {
    pub cell: crate::transcript::Cell,
    pub scroll: usize,
}

#[derive(Debug, Clone)]
pub struct ThemePicker {
    pub original: crate::theme::Theme,
    pub selected: usize,
}

#[derive(Debug, Clone, Default)]
pub struct HarnessCapabilities {
    pub models: bool,
    pub permissions: bool,
    pub plan_mode: bool,
    pub compaction: bool,
    pub jobs: bool,
    pub user_questions: bool,
    pub session_search: bool,
    pub commands: bool,
    pub tools: bool,
    /// Is the session-projection registry mounted? Without it the goal bar,
    /// context pressure and todo sync all go quiet, so `/context` reports it.
    pub projections: bool,
    pub goals: bool,
}

#[derive(Debug, Clone)]
pub struct HarnessCommand {
    pub name: String,
    pub description: String,
    pub input_hint: Option<String>,
}
#[derive(Debug, Clone)]
pub struct PaletteRow {
    pub label: String,
    pub action: String,
    pub shortcut: Option<String>,
    pub section: &'static str,
}

#[derive(Debug, Clone)]
pub struct Palette {
    pub rows: Vec<PaletteRow>,
    pub filter: String,
    pub selected: usize,
    pub visible: Vec<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutSection {
    Essentials,
    Input,
    Navigation,
    Actions,
    Panels,
    Session,
}

impl ShortcutSection {
    pub const ALL: [Self; 6] = [
        Self::Essentials,
        Self::Input,
        Self::Navigation,
        Self::Actions,
        Self::Panels,
        Self::Session,
    ];

    pub const fn title(self) -> &'static str {
        match self {
            Self::Essentials => "Essentials",
            Self::Input => "Input",
            Self::Navigation => "Conversation Navigation",
            Self::Actions => "Conversation Actions",
            Self::Panels => "Panels",
            Self::Session => "Session",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ShortcutRow {
    pub label: &'static str,
    pub keys: &'static str,
    pub section: ShortcutSection,
}

#[derive(Debug, Clone)]
pub struct Shortcuts {
    pub rows: Vec<ShortcutRow>,
    pub filter: String,
    pub selected_section: usize,
    pub expanded: [bool; 6],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionMode {
    Normal,
    Plan,
    AlwaysApprove,
}

impl PermissionMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Normal => "Normal",
            Self::Plan => "Plan",
            Self::AlwaysApprove => "Always-approve",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReasoningEffortEntry {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ModelReasoning {
    pub efforts: Vec<ReasoningEffortEntry>,
    pub default_effort: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ModelEntry {
    pub provider: String,
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub context_window: Option<u64>,
    pub reasoning: Option<ModelReasoning>,
}

#[derive(Debug, Clone)]
pub struct RewindEntry {
    pub seq: u64,
    pub preview: String,
}

#[derive(Debug, Clone)]
pub struct RewindPicker {
    pub items: Vec<RewindEntry>,
    pub selected: usize,
}

#[derive(Debug, Clone)]
pub struct FilePicker {
    pub files: Vec<String>,
    pub query: String,
    pub selected: usize,
    pub visible: Vec<usize>,
}

#[derive(Debug, Clone)]
pub struct ModelPicker {
    pub rows: Vec<ModelEntry>,
    pub filter: String,
    pub selected: usize,
}

#[derive(Debug, Clone)]
pub struct EffortPicker {
    pub model: ModelEntry,
    pub rows: Vec<ReasoningEffortEntry>,
    pub selected: usize,
}

impl ModelPicker {
    pub fn visible(&self) -> Vec<usize> {
        let f = self.filter.to_lowercase();
        (0..self.rows.len())
            .filter(|i| {
                let r = &self.rows[*i];
                f.is_empty()
                    || r.id.to_lowercase().contains(&f)
                    || r.name.to_lowercase().contains(&f)
                    || r.provider.to_lowercase().contains(&f)
            })
            .collect()
    }
}

/// 渲染期记录的弹窗可点击行（ui 每帧重填，handle_mouse 命中测试用）。
/// 点击等价于"方向键移到该项 + 回车"，与键盘行为完全一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogHit {
    /// 选中第 index 项并确认（语义随弹窗：执行/选择/展开）。
    Select {
        row: u16,
        col_start: u16,
        col_end: u16,
        index: usize,
    },
    /// 点击等价于按下字符键（y/n 确认框）。
    Key {
        row: u16,
        col_start: u16,
        col_end: u16,
        ch: char,
    },
}

impl DialogHit {
    fn contains(&self, column: u16, row: u16) -> bool {
        let (hit_row, start, end) = match self {
            DialogHit::Select {
                row,
                col_start,
                col_end,
                ..
            }
            | DialogHit::Key {
                row,
                col_start,
                col_end,
                ..
            } => (*row, *col_start, *col_end),
        };
        row == hit_row && column >= start && column < end
    }
}

#[derive(Debug, Clone)]
pub enum Dialog {
    None,
    Approval(ApprovalDialog),
    Ask(AskDialog),
    Resume(ResumePicker),
    Model(ModelPicker),
    Effort(EffortPicker),
    FilePicker(FilePicker),
    Rewind(RewindPicker),
    Palette(Palette),
    Shortcuts(Shortcuts),
    Info(InfoDialog),
    Theme(ThemePicker),
    Subagent(SubagentView),
    Tasks(TasksView),
    Todos(TodosView),
    Block(BlockView),
    History(HistoryView),
    Queue(QueueView),
    Provider(ProviderWizard),
    ProviderList(ProviderListView),
    ProviderKey(ProviderKeyDialog),
    ProviderRemove(String),
}

/// /provider add 的分支向导（Dialog::Provider）。交互原型见
/// docs/prototypes/provider-setup.html：类型（内置目录/自定义，OAuth 此
/// 构建不支持故灰置）→ 连接（自定义要协议+baseURL）→ 凭据 → 模型（拉取
/// 或手填，逐模型配多模态/思考强度）→ 确认。
///
/// 协议词表与内置目录列表都来自桥端（llm-pi-ai schema 的 providers.*.api
/// 并集 / pi-ai builtinProviders），与本端校验和安装目录同源，不漂移。
#[derive(Debug, Clone)]
pub struct ProviderWizard {
    pub(crate) step: ProviderStep,
    pub(crate) kind: Option<WizardKind>,
    /// Type 步的选项光标：0 = 内置目录，1 = 自定义。
    pub(crate) type_sel: usize,
    /// 桥端下发的内置目录 provider 列表（Known 步的选项）。
    pub(crate) catalog: Vec<CatalogProvider>,
    pub(crate) catalog_sel: usize,
    pub(crate) protocols: Vec<String>,
    pub(crate) proto_sel: usize,
    /// 当前文本步骤的编辑缓冲（步进时与字段互换）。
    pub(crate) draft: String,
    pub(crate) id: String,
    pub(crate) api: String,
    pub(crate) base_url: String,
    pub(crate) api_key: String,
    pub(crate) models: Vec<ModelDraft>,
    pub(crate) model_cursor: usize,
    pub(crate) models_focus: ModelsFocus,
    /// 展开行内的列光标：0 = 多模态开关，1 = 思考开关，2.. = 级别 chips。
    pub(crate) detail_col: usize,
    /// 手填模型 id 的输入缓冲（Manual 焦点）。
    pub(crate) manual: String,
    /// 模型拉取请求在飞（桥端 GET {baseURL}/models）。
    pub fetching: bool,
    pub saving: bool,
    pub error: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum WizardKind {
    Known,
    Custom,
}

/// Models 步内部的焦点：列表导航 / 手填输入 / 展开行详情。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ModelsFocus {
    List,
    Manual,
    Detail,
}

/// 思考强度级别：(配置 id, 中文标签)。写入 reasoningEfforts 的 identity map。
pub(crate) const EFFORT_LEVELS: [(&str, &str); 6] = [
    ("low", "低"),
    ("medium", "中"),
    ("high", "高"),
    ("xhigh", "超高"),
    ("max", "最高"),
    ("ultra", "极致"),
];

#[derive(Debug, Clone)]
pub struct ModelDraft {
    pub id: String,
    pub included: bool,
    pub vision: bool,
    pub reasoning: bool,
    pub efforts: Vec<String>,
    pub open: bool,
}

#[derive(Debug, Clone)]
pub struct CatalogProvider {
    pub id: String,
    pub name: String,
    pub key_page: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ProviderStep {
    Type,
    Known,
    Id,
    Api,
    BaseUrl,
    ApiKey,
    Models,
    Confirm,
}

/// /provider 的服务商管理面板（Dialog::ProviderList）。
#[derive(Debug, Clone)]
pub struct ProviderListView {
    pub items: Vec<ProviderItem>,
    pub selected: usize,
}

#[derive(Debug, Clone)]
pub struct ProviderItem {
    pub id: String,
    /// "stored"（凭据文件）| "env"（环境变量）| "none"。
    pub key: String,
}

/// /provider 列表里按 e 弹出的单行 key 输入（Dialog::ProviderKey）。
#[derive(Debug, Clone)]
pub struct ProviderKeyDialog {
    pub id: String,
    pub draft: String,
    pub saving: bool,
    pub error: Option<String>,
}

/// 与桥端 PROVIDER_ROUTE_PATTERN 同一规则：^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$。
fn valid_provider_id(id: &str) -> bool {
    let mut chars = id.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    let mut segment_filled = true;
    for c in chars {
        match c {
            'a'..='z' | '0'..='9' => segment_filled = true,
            '-' if segment_filled => segment_filled = false,
            _ => return false,
        }
    }
    segment_filled
}

impl ProviderStep {
    pub(crate) fn label(self) -> &'static str {
        match self {
            ProviderStep::Type => "type",
            ProviderStep::Known => "catalog",
            ProviderStep::Id => "id",
            ProviderStep::Api => "protocol",
            ProviderStep::BaseUrl => "base URL",
            ProviderStep::ApiKey => "API key",
            ProviderStep::Models => "models",
            ProviderStep::Confirm => "confirm",
        }
    }

    /// 当前文本步骤的空值提示（渲染在值为空时）。
    pub(crate) fn empty_hint(self) -> &'static str {
        match self {
            ProviderStep::Id => "required, e.g. my-provider",
            ProviderStep::Api => "required",
            ProviderStep::BaseUrl => "required for custom routes",
            ProviderStep::ApiKey => "empty = skip",
            _ => "",
        }
    }
}

impl ProviderWizard {
    pub(crate) fn new(protocols: Vec<String>, catalog: Vec<CatalogProvider>) -> Self {
        Self {
            step: ProviderStep::Type,
            kind: None,
            type_sel: 0,
            catalog,
            catalog_sel: 0,
            protocols,
            proto_sel: 0,
            draft: String::new(),
            id: String::new(),
            api: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            models: Vec::new(),
            model_cursor: 0,
            models_focus: ModelsFocus::List,
            detail_col: 0,
            manual: String::new(),
            fetching: false,
            saving: false,
            error: None,
        }
    }

    /// kind 决定的步骤序列（Type 之后分叉；known 免连接与模型步——
    /// 协议/baseURL/models 全由安装目录供给）。
    pub(crate) fn sequence(&self) -> &'static [ProviderStep] {
        match self.kind {
            Some(WizardKind::Known) => &[
                ProviderStep::Type,
                ProviderStep::Known,
                ProviderStep::ApiKey,
                ProviderStep::Confirm,
            ],
            _ => &[
                ProviderStep::Type,
                ProviderStep::Id,
                ProviderStep::Api,
                ProviderStep::BaseUrl,
                ProviderStep::ApiKey,
                ProviderStep::Models,
                ProviderStep::Confirm,
            ],
        }
    }

    /// 字段一览里要回显的文本字段（known 只显示 key）。
    pub(crate) fn visible_fields(&self) -> &'static [ProviderStep] {
        match self.kind {
            Some(WizardKind::Known) => &[ProviderStep::ApiKey],
            _ => &[
                ProviderStep::Id,
                ProviderStep::Api,
                ProviderStep::BaseUrl,
                ProviderStep::ApiKey,
            ],
        }
    }

    pub(crate) fn value(&self, step: ProviderStep) -> &str {
        match step {
            ProviderStep::Id => &self.id,
            ProviderStep::Api => &self.api,
            ProviderStep::BaseUrl => &self.base_url,
            ProviderStep::ApiKey => &self.api_key,
            _ => "",
        }
    }

    /// 当前步骤是否吃文本输入（Models 步由 models_focus 另行裁决）。
    pub(crate) fn editing_text(&self) -> bool {
        match self.step {
            ProviderStep::Id | ProviderStep::BaseUrl | ProviderStep::ApiKey => true,
            ProviderStep::Api => self.protocols.is_empty(),
            _ => false,
        }
    }

    /// 校验草稿并提交进当前字段；Err 是展示在面板里的校验信息。
    pub(crate) fn commit_draft(&mut self) -> Result<(), String> {
        let text = self.draft.trim().to_string();
        match self.step {
            ProviderStep::Id => {
                if !valid_provider_id(&text) {
                    return Err("id must be lowercase segments, e.g. my-provider".to_string());
                }
                self.id = text;
            }
            ProviderStep::Api => {
                if text.is_empty() {
                    return Err("protocol required".to_string());
                }
                self.api = text;
            }
            ProviderStep::BaseUrl => {
                if !text.starts_with("https://") && !text.starts_with("http://") {
                    return Err("base URL must start with https:// or http://".to_string());
                }
                self.base_url = text;
            }
            ProviderStep::ApiKey => self.api_key = text,
            _ => {}
        }
        Ok(())
    }

    /// 步进到序列下一步；文本步把已有值装入草稿。
    pub(crate) fn advance(&mut self) {
        let seq = self.sequence();
        let Some(pos) = seq.iter().position(|s| *s == self.step) else {
            return;
        };
        let Some(next) = seq.get(pos + 1) else { return };
        self.step = *next;
        self.draft = self.value(*next).to_string();
        self.error = None;
    }

    /// 回退序列上一步。
    pub(crate) fn back(&mut self) {
        if self.step == ProviderStep::Models && self.models_focus != ModelsFocus::List {
            self.models_focus = ModelsFocus::List;
            return;
        }
        let seq = self.sequence();
        let Some(pos) = seq.iter().position(|s| *s == self.step) else {
            return;
        };
        if pos == 0 {
            return;
        }
        let prev = seq[pos - 1];
        self.step = prev;
        self.draft = self.value(prev).to_string();
        self.error = None;
    }

    /// 保存用 draft JSON：known 只给 key（桥端 known 形态），custom 给全量。
    pub(crate) fn save_payload(&self) -> Value {
        match self.kind {
            Some(WizardKind::Known) => json!({
                "id": self.id,
                "apiKey": self.api_key,
                "known": true,
            }),
            _ => json!({
                "id": self.id,
                "api": self.api,
                "baseURL": self.base_url,
                "apiKey": self.api_key,
                "models": self.models.iter()
                    .filter(|m| m.included)
                    .map(|m| json!({
                        "id": m.id,
                        "vision": m.vision,
                        "efforts": if m.reasoning { m.efforts.clone() } else { Vec::new() },
                    }))
                    .collect::<Vec<_>>(),
            }),
        }
    }

    /// ↑/↓ 移动模型光标后保持焦点一致：Detail 跟随到新行（未展开则回 List）。
    pub(crate) fn sync_models_focus(&mut self) {
        if self.models_focus == ModelsFocus::Detail {
            match self.models.get(self.model_cursor) {
                Some(model) if model.open => self.detail_col = 0,
                _ => self.models_focus = ModelsFocus::List,
            }
        }
    }
}

/// Models 步展开行里的列切换：0 = 多模态开关，1 = 思考开关（开启时默认
/// 低/中/高三档），2.. = 级别 chips 多选。
fn wizard_toggle_detail(wizard: &mut ProviderWizard) {
    let Some(model) = wizard.models.get_mut(wizard.model_cursor) else {
        return;
    };
    match wizard.detail_col {
        0 => model.vision = !model.vision,
        1 => {
            model.reasoning = !model.reasoning;
            if model.reasoning && model.efforts.is_empty() {
                model.efforts = vec!["medium".into(), "high".into()];
            }
        }
        col => {
            let Some((level, _)) = EFFORT_LEVELS.get(col - 2) else {
                return;
            };
            if model.efforts.iter().any(|e| e == level) {
                model.efforts.retain(|e| e != level);
            } else {
                model.efforts.push((*level).to_string());
            }
            model.reasoning = !model.efforts.is_empty();
        }
    }
}

/// 在系统浏览器打开 URL（macOS open / 其他 xdg-open），失败静默——
/// 面板里仍显示着地址，用户可手开。
fn open_url(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener).arg(url).spawn();
}

pub struct App {
    pub theme: Theme,
    pub transcript: Transcript,
    /// DSH session projections (docs/04 section 3) — the canonical read model.
    pub projections: crate::projection::Projections,
    pub input: String,
    /// Caret as a byte offset into `input`. Always on a UTF-8 boundary and
    /// never past the end — `clamp_cursor` restores that before every edit,
    /// so a stale write to `input` can never split a codepoint.
    pub cursor: usize,
    /// Snapshots for Ctrl+Z. Consecutive plain inserts coalesce into one entry
    /// so undo steps over a typing burst, not one character.
    undo: Vec<(String, usize)>,
    typing_run: bool,
    confirm: Option<(Confirm, Instant)>,
    /// Is `notice` currently owned by a confirm prompt? Only then may `tick`
    /// clear it, so an unrelated notice is never eaten.
    confirm_notice: bool,
    pub history: Vec<String>,
    pub history_cursor: Option<usize>,
    pub history_draft: String,
    pub multiline: bool,
    pub focus: Focus,
    pub state: RunState,
    pub esc: EscArm,
    pub session_id: String,
    pub provider: String,
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub context_window: Option<u64>,
    pub reasoning_effort_name: Option<String>,
    pub status: String,
    pub notice: Option<String>,
    /// When the current notice was posted; `tick` clears it past NOTICE_TTL_MS.
    notice_at: Option<Instant>,
    pub scroll: usize,
    /// 视口是否跟随选中块（draw_scrollback 用它决定滚动画窗）。
    pub follow_selection: bool,
    pub composer_top: u16,
    /// 绘制时记录的 composer 盒几何，鼠标点击定位光标用（ui.rs draw 每帧刷新）。
    pub composer_bottom: u16,
    pub composer_inner_width: u16,
    /// 本帧弹窗的可点击行（ui::draw_dialog 每帧重填；无弹窗时清空）。
    pub dialog_hits: Vec<DialogHit>,
    pub needs_redraw: bool,
    /// Is mouse reporting on? Defaults to ON.
    ///
    /// Mouse reporting (`?1000h` + `?1006h`) is on by default so the wheel
    /// scrolls the transcript (docs/01 section 2.7). With reporting off the
    /// terminal turns the wheel into arrow keys, which the composer reads as
    /// prompt-history recall — history text landing in the draft. `/mouse`
    /// opts out for terminals where Shift+drag selection does not work.
    pub mouse_capture: bool,
    /// Set when `mouse_capture` changed and the event loop must re-issue the
    /// escape sequence.
    pub mouse_capture_dirty: bool,
    pub quit: bool,
    pub demo: bool,
    pub dialog: Dialog,
    pub workspace: String,
    pub term_kind: TermKind,
    pub in_tmux: bool,
    at_fragment_start: Option<usize>,
    permission_presets: Vec<String>,
    preset_index: usize,
    pub permission_mode: PermissionMode,
    catalog_for_presets: bool,
    /// 只有 /model、Ctrl+M 这类用户主动发起的 FetchCatalog 才应在结果到达时
    /// 弹出模型选择器；启动（tui/ready）和 capabilities-changed 的后台拉取
    /// 只更新数据，否则每次启动都会强制弹一次模型选择。
    catalog_for_picker: bool,
    /// 桥端 initialize/list-providers 下发的协议词表（llm-pi-ai schema 的
    /// providers.*.api 并集）；空 = 桥未下发，向导协议步骤退化为自由文本。
    provider_protocols: Vec<String>,
    /// 桥端 list-providers 下发的内置目录 provider 预设（pi-ai
    /// builtinProviders）；空 = 未拉取，/provider add 会触发一次拉取。
    catalog_providers: Vec<CatalogProvider>,
    live_ids: HashSet<String>,
    capabilities: HarnessCapabilities,
    harness_commands: Vec<HarnessCommand>,
    catalog_loaded: bool,
    cancel_grace: Option<Instant>,
    pub child_transcripts: HashMap<String, Transcript>,
    pending_resume_file: Option<std::path::PathBuf>,
    queue: Vec<QueueItem>,
    cmd_tx: Sender<Cmd>,
}

impl App {
    pub fn new(
        theme: Theme,
        session_id: String,
        provider: String,
        model: String,
        demo: bool,
        cmd_tx: Sender<Cmd>,
        workspace: String,
    ) -> Self {
        Self {
            theme,
            transcript: Transcript::new(),
            projections: crate::projection::Projections::default(),
            input: String::new(),
            cursor: 0,
            undo: Vec::new(),
            typing_run: false,
            confirm: None,
            confirm_notice: false,
            history: Vec::new(),
            history_cursor: None,
            history_draft: String::new(),
            multiline: false,
            focus: Focus::Prompt,
            state: RunState::Idle,
            esc: EscArm::None,
            session_id,
            provider,
            model,
            reasoning_effort: None,
            context_window: None,
            reasoning_effort_name: None,
            status: String::new(),
            notice: None,
            notice_at: None,
            scroll: 0,
            follow_selection: true,
            composer_top: 0,
            composer_bottom: 0,
            composer_inner_width: 1,
            dialog_hits: Vec::new(),
            needs_redraw: true,
            mouse_capture: true,
            // Applied on the first loop pass, so the default holds whatever it is.
            mouse_capture_dirty: true,
            quit: false,
            demo,
            dialog: Dialog::None,
            workspace,
            term_kind: TermKind::Plain,
            in_tmux: false,
            at_fragment_start: None,
            permission_presets: Vec::new(),
            preset_index: 0,
            permission_mode: PermissionMode::Normal,
            catalog_for_presets: false,
            catalog_for_picker: false,
            provider_protocols: Vec::new(),
            catalog_providers: Vec::new(),
            capabilities: HarnessCapabilities::default(),
            harness_commands: Vec::new(),
            catalog_loaded: false,
            live_ids: HashSet::new(),
            cancel_grace: None,
            child_transcripts: HashMap::new(),
            pending_resume_file: None,
            queue: Vec::new(),
            cmd_tx,
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(self.state, RunState::Running | RunState::Starting)
    }

    pub fn queue(&self) -> &[QueueItem] {
        &self.queue
    }
    fn queued_items(&self) -> impl Iterator<Item = &QueueItem> {
        self.queue.iter().filter(|item| item.placement == "queued")
    }

    pub fn queued_count(&self) -> usize {
        self.queued_items().count()
    }

    fn open_queue(&mut self) {
        if self.queued_count() == 0 {
            self.set_notice("queue empty");
            return;
        }
        self.dialog = Dialog::Queue(QueueView {
            selected: 0,
            editing: false,
            draft: String::new(),
        });
    }

    /// The `todos` projection is authoritative when present (docs/04 section 5).
    /// The tool-argument parser in `transcript.rs` stays as the fallback for
    /// harnesses that publish no projection, so this only ever overwrites the
    /// transcript's guess with the real contract.
    fn sync_todos_from_projection(&mut self) {
        if let Some(items) = self.projections.todos.clone() {
            self.transcript.todos = items;
            if let Dialog::Todos(view) = &mut self.dialog {
                view.selected = view
                    .selected
                    .min(self.transcript.todos.len().saturating_sub(1));
            }
            if self.transcript.todos.is_empty() {
                if let Dialog::Todos(_) = self.dialog {
                    self.dialog = Dialog::None;
                    self.set_notice("task list cleared");
                }
            }
        }
    }

    /// `/context` rows built from DSH's token-meter projections (docs/04 6.4).
    ///
    /// The breakdown figures are the meter's fixed-density estimates, which
    /// systematically underprice CJK text and JSON schemas. Upstream is explicit
    /// that they are composition, never a total — so they are labelled `~` and
    /// deliberately not summed. The one trustworthy total is
    /// `contextPressure.projectedTokens`, reported separately as `context`.
    fn context_rows(&self) -> Vec<(String, String)> {
        let mut rows = Vec::new();
        // Report the seam's presence first. Without it every row below is
        // absent, and "no rows" is indistinguishable from "nothing happened
        // yet" — which is exactly the confusion a smoke test hits.
        if !self.demo {
            // Name the keys. "none received yet" could not distinguish a silent
            // pipeline from one delivering nulls on a fresh session, which is
            // exactly the ambiguity that made a real report unreadable.
            rows.push((
                "projections".to_string(),
                if self.projections.seen.is_empty() {
                    if self.capabilities.projections {
                        "mounted · 0 keys received".to_string()
                    } else {
                        "NOT MOUNTED".to_string()
                    }
                } else {
                    format!(
                        "{} keys @seq {} · {}",
                        self.projections.seen.len(),
                        self.projections.seq,
                        self.projections
                            .seen
                            .iter()
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(" ")
                    )
                },
            ));
            // ctx.goals is what a future goal write path needs; report whether
            // it is reachable so the answer is known before it is relied on.
            rows.push((
                "goal service".to_string(),
                if self.capabilities.goals {
                    "available".to_string()
                } else {
                    "absent".to_string()
                },
            ));
        }
        if let Some(p) = self.projections.context_pressure {
            let fmt = |v: Option<u64>| match v {
                Some(n) => n.to_string(),
                None => "-".to_string(),
            };
            rows.push((
                "context (next req)".to_string(),
                match (p.projected_tokens, p.context_window) {
                    (Some(used), Some(window)) if window > 0 => format!(
                        "{used} / {window} · {}%",
                        (used as f64 / window as f64 * 100.0).round() as u64
                    ),
                    _ => fmt(p.projected_tokens),
                },
            ));
            rows.push(("context (last req)".to_string(), fmt(p.pressure_tokens)));
        }
        if let Some(b) = self.projections.context_breakdown {
            // `~` marks these as estimates, and they are listed individually so
            // no reader is invited to add them up.
            rows.push(("~ system prompt".to_string(), b.system_tokens.to_string()));
            rows.push(("~ tool schemas".to_string(), b.tools_tokens.to_string()));
            rows.push(("~ conversation".to_string(), b.message_tokens.to_string()));
        }
        if let Some(goal) = self.projections.goal.as_ref() {
            rows.push((
                "goal".to_string(),
                format!(
                    "{} · round {}/{}",
                    goal.objective, goal.rounds_started, goal.max_rounds
                ),
            ));
        }
        // Projections we do not model yet still surface here, so a capability a
        // future plugin adds is visible without a code change. Values are stored
        // WHOLE — `y` copies these rows, and a truncated JSON blob is useless.
        // The card truncates for display only.
        for (key, value) in &self.projections.extra {
            rows.push((key.clone(), flatten_value(value)));
        }
        rows
    }

    fn open_todos(&mut self) {
        if self.transcript.todos.is_empty() {
            self.set_notice("no task list yet");
            return;
        }
        // Land on whatever the agent is working on right now.
        let selected = self
            .transcript
            .todos
            .iter()
            .position(|item| item.status == crate::transcript::TodoStatus::InProgress)
            .unwrap_or(0);
        self.dialog = Dialog::Todos(TodosView { selected });
    }

    fn queue_action(&mut self, item_id: String, action: Value) {
        if action.get("kind").and_then(Value::as_str) == Some("edit") {
            if let Some(item) = self.queue.iter_mut().find(|item| item.id == item_id) {
                let text = action
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                item.preview = queue_preview(&text);
                item.text = Some(text);
            }
        } else if matches!(
            action.get("kind").and_then(Value::as_str),
            Some("remove" | "steer" | "send-now")
        ) {
            self.queue.retain(|item| item.id != item_id);
        }
        let _ = self.cmd_tx.send(Cmd::UpdateQueue {
            session_id: self.session_id.clone(),
            item_id,
            action,
        });
    }

    fn send_now_item(&mut self, item_id: String) {
        if !self.is_running() {
            return;
        }
        self.queue_action(item_id, json!({ "kind": "send-now" }));
        self.status = "sending queued follow-up now".into();
    }

    fn send_now_text(&mut self, text: String) {
        if text.trim().is_empty() || !self.is_running() {
            return;
        }
        self.history.push(text.clone());
        self.clear_input();
        self.leave_history_navigation();
        let _ = self.cmd_tx.send(Cmd::SendNow {
            session_id: self.session_id.clone(),
            text,
        });
        self.status = "sending follow-up now".into();
    }

    fn replace_queue(&mut self, params: &Value) {
        let sid = params
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if sid != self.session_id {
            return;
        }
        self.queue = params
            .get("items")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(queue_item_from_value).collect())
            .unwrap_or_default();
        let count = self.queued_count();
        if let Dialog::Queue(view) = &mut self.dialog {
            if count == 0 {
                self.dialog = Dialog::None;
            } else {
                view.selected = view.selected.min(count - 1);
            }
        }
    }

    pub fn has_dialog(&self) -> bool {
        !matches!(self.dialog, Dialog::None)
    }

    fn respond(&mut self, id: String, result: Value) {
        let _ = self.cmd_tx.send(Cmd::Respond { id, result });
    }

    fn send_input(&mut self, text: String) {
        if text.trim().is_empty() {
            return;
        }
        self.history.push(text.clone());
        // `text` already came out of `take_input`; this only covers callers
        // that hand us a string built some other way.
        self.input.clear();
        self.cursor = 0;
        self.typing_run = false;
        self.history_cursor = None;
        self.history_draft.clear();
        self.state = RunState::Starting;
        if self.demo {
            self.transcript.push(
                CellKind::Assistant,
                String::new(),
                "(demo) 收到，这里是本地回显。".to_string(),
            );

            self.state = RunState::Idle;
            return;
        }
        let _ = self.cmd_tx.send(Cmd::Prompt {
            session_id: self.session_id.clone(),
            text,
        });
    }

    fn history_matches(&self, query: &str) -> Vec<usize> {
        history_matches(&self.history, query)
    }

    fn open_history(&mut self) {
        if self.history.is_empty() {
            self.set_notice("prompt history empty");
            return;
        }
        let visible = self.history_matches("");
        self.dialog = Dialog::History(HistoryView {
            query: String::new(),
            selected: 0,
            visible,
        });
    }
    // ---- composer editing (docs/01 section 6) ---------------------------
    // `input` is a plain String and the caret lives beside it in `cursor` as a
    // byte offset. Every mutation runs `clamp_cursor` first, so a stale write
    // to `input` (history restore, a test) can never split a codepoint.

    fn clamp_cursor(&mut self) {
        if self.cursor > self.input.len() {
            self.cursor = self.input.len();
            return;
        }
        while self.cursor < self.input.len() && !self.input.is_char_boundary(self.cursor) {
            self.cursor += 1;
        }
    }

    /// Snapshot for Ctrl+Z. `grouped` marks a plain insert or delete, which
    /// coalesces into the run before it so undo steps over a typing burst
    /// rather than one character.
    fn push_undo(&mut self, grouped: bool) {
        if grouped && self.typing_run {
            return;
        }
        if self.undo.last().map(|(text, _)| text.as_str()) != Some(self.input.as_str()) {
            self.undo.push((self.input.clone(), self.cursor));
            if self.undo.len() > 64 {
                self.undo.remove(0);
            }
        }
        self.typing_run = grouped;
    }

    fn undo_input(&mut self) {
        let Some((text, cursor)) = self.undo.pop() else {
            self.set_notice("nothing to undo");
            return;
        };
        self.input = text;
        self.cursor = cursor;
        self.typing_run = false;
        self.clamp_cursor();
        self.leave_history_navigation();
    }

    /// Replace the whole draft and park the caret at the end (history recall,
    /// queue edit, tests).
    pub fn set_input(&mut self, text: impl Into<String>) {
        self.input = text.into();
        self.cursor = self.input.len();
        self.typing_run = false;
    }

    fn take_input(&mut self) -> String {
        self.push_undo(false);
        self.cursor = 0;
        std::mem::take(&mut self.input)
    }

    fn clear_input(&mut self) {
        if self.input.is_empty() {
            return;
        }
        self.push_undo(false);
        self.input.clear();
        self.cursor = 0;
    }

    fn insert_str(&mut self, text: &str) {
        self.clamp_cursor();
        self.push_undo(true);
        self.input.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    fn insert_char(&mut self, c: char) {
        self.clamp_cursor();
        self.push_undo(true);
        self.input.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    fn prev_boundary(&self, from: usize) -> usize {
        let mut i = from.saturating_sub(1);
        while i > 0 && !self.input.is_char_boundary(i) {
            i -= 1;
        }
        i
    }

    fn next_boundary(&self, from: usize) -> usize {
        let mut i = (from + 1).min(self.input.len());
        while i < self.input.len() && !self.input.is_char_boundary(i) {
            i += 1;
        }
        i
    }

    fn is_word_char(c: char) -> bool {
        c.is_alphanumeric() || c == '_'
    }

    /// readline word motion: skip the whitespace beside the caret, then the
    /// run of word characters (or the run of punctuation) it lands in.
    fn word_start_before(&self, mut at: usize) -> usize {
        let text = self.input.as_str();
        let prev = |i: usize| text[..i].chars().next_back().map(|c| (i - c.len_utf8(), c));
        while let Some((i, c)) = prev(at) {
            if !c.is_whitespace() {
                break;
            }
            at = i;
        }
        let Some((_, anchor)) = prev(at) else {
            return at;
        };
        let want_word = Self::is_word_char(anchor);
        while let Some((i, c)) = prev(at) {
            if c.is_whitespace() || Self::is_word_char(c) != want_word {
                break;
            }
            at = i;
        }
        at
    }

    fn word_end_after(&self, mut at: usize) -> usize {
        let text = self.input.as_str();
        let next = |i: usize| text[i..].chars().next().map(|c| (i + c.len_utf8(), c));
        while let Some((i, c)) = next(at) {
            if !c.is_whitespace() {
                break;
            }
            at = i;
        }
        let Some((_, anchor)) = next(at) else {
            return at;
        };
        let want_word = Self::is_word_char(anchor);
        while let Some((i, c)) = next(at) {
            if c.is_whitespace() || Self::is_word_char(c) != want_word {
                break;
            }
            at = i;
        }
        at
    }

    fn line_start(&self, at: usize) -> usize {
        self.input[..at].rfind('\n').map(|i| i + 1).unwrap_or(0)
    }

    fn line_end(&self, at: usize) -> usize {
        self.input[at..]
            .find('\n')
            .map(|i| at + i)
            .unwrap_or(self.input.len())
    }

    fn cursor_left(&mut self) {
        self.clamp_cursor();
        self.cursor = self.prev_boundary(self.cursor);
        self.typing_run = false;
    }

    fn cursor_right(&mut self) {
        self.clamp_cursor();
        self.cursor = self.next_boundary(self.cursor);
        self.typing_run = false;
    }

    fn cursor_word_left(&mut self) {
        self.clamp_cursor();
        self.cursor = self.word_start_before(self.cursor);
        self.typing_run = false;
    }

    fn cursor_word_right(&mut self) {
        self.clamp_cursor();
        self.cursor = self.word_end_after(self.cursor);
        self.typing_run = false;
    }

    fn cursor_line_start(&mut self) {
        self.clamp_cursor();
        self.cursor = self.line_start(self.cursor);
        self.typing_run = false;
    }

    fn cursor_line_end(&mut self) {
        self.clamp_cursor();
        self.cursor = self.line_end(self.cursor);
        self.typing_run = false;
    }

    /// Up/Down inside a multi-line draft, holding the column across the move.
    /// Returns false when there is no line to move onto, so the caller can
    /// fall back to history navigation.
    fn move_cursor_line(&mut self, delta: i32) -> bool {
        self.clamp_cursor();
        if !self.input.contains('\n') {
            return false;
        }
        let start = self.line_start(self.cursor);
        let col = self.input[start..self.cursor].chars().count();
        let target_start = if delta < 0 {
            if start == 0 {
                return false;
            }
            self.line_start(start - 1)
        } else {
            let end = self.line_end(self.cursor);
            if end >= self.input.len() {
                return false;
            }
            end + 1
        };
        let target_end = self.line_end(target_start);
        let mut at = target_start;
        for _ in 0..col {
            if at >= target_end {
                break;
            }
            at = self.next_boundary(at);
        }
        self.cursor = at.min(target_end);
        self.typing_run = false;
        true
    }

    fn backspace(&mut self) {
        self.clamp_cursor();
        if self.cursor == 0 {
            return;
        }
        let start = self.prev_boundary(self.cursor);
        self.push_undo(true);
        self.input.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    fn delete_forward(&mut self) {
        self.clamp_cursor();
        if self.cursor >= self.input.len() {
            return;
        }
        let end = self.next_boundary(self.cursor);
        self.push_undo(true);
        self.input.replace_range(self.cursor..end, "");
    }

    fn delete_word_before(&mut self) {
        self.clamp_cursor();
        let start = self.word_start_before(self.cursor);
        if start == self.cursor {
            return;
        }
        self.push_undo(false);
        self.input.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    fn delete_word_after(&mut self) {
        self.clamp_cursor();
        let end = self.word_end_after(self.cursor);
        if end == self.cursor {
            return;
        }
        self.push_undo(false);
        self.input.replace_range(self.cursor..end, "");
    }

    fn delete_to_line_start(&mut self) {
        self.clamp_cursor();
        let start = self.line_start(self.cursor);
        if start == self.cursor {
            return;
        }
        self.push_undo(false);
        self.input.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    fn delete_to_line_end(&mut self) {
        self.clamp_cursor();
        let end = self.line_end(self.cursor);
        if end == self.cursor {
            return;
        }
        self.push_undo(false);
        self.input.replace_range(self.cursor..end, "");
    }

    fn history_previous(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.history_cursor {
            None => {
                self.history_draft = self.input.clone();
                self.history.len() - 1
            }
            Some(index) => index.saturating_sub(1),
        };
        self.history_cursor = Some(next);
        self.set_input(self.history[next].clone());
    }

    fn history_next(&mut self) {
        let Some(index) = self.history_cursor else {
            return;
        };
        if index + 1 < self.history.len() {
            let next = index + 1;
            self.history_cursor = Some(next);
            self.set_input(self.history[next].clone());
        } else {
            self.history_cursor = None;
            let draft = std::mem::take(&mut self.history_draft);
            self.set_input(draft);
        }
    }

    fn leave_history_navigation(&mut self) {
        self.history_cursor = None;
        self.history_draft.clear();
    }

    fn set_selected_fold(&mut self, folded: bool) {
        if let Some(index) = self.transcript.selected {
            if let Some(cell) = self.transcript.cells.get_mut(index) {
                cell.folded = folded;
            }
        }
    }

    /// Is this destructive action already armed and still inside the window?
    /// Consumes the arm either way, so a stale arm never fires later.
    fn armed(&mut self, what: Confirm) -> bool {
        let hit = matches!(self.confirm, Some((w, t)) if w == what && t.elapsed().as_millis() <= CONFIRM_MS);
        self.confirm = None;
        self.confirm_notice = false;
        hit
    }

    fn arm(&mut self, what: Confirm) {
        self.confirm = Some((what, Instant::now()));
        self.confirm_notice = true;
    }

    /// Post a transient notice with its timestamp; `tick` fades it.
    fn set_notice(&mut self, text: impl Into<String>) {
        self.notice = Some(text.into());
        self.notice_at = Some(Instant::now());
    }

    /// Called from the event loop's idle tick. Drops an expired confirm arm and
    /// the prompt that went with it — leaving "press again to quit" on screen
    /// after the window closed would be a lie.
    pub fn tick(&mut self) {
        let expired = matches!(self.confirm, Some((_, t)) if t.elapsed().as_millis() > CONFIRM_MS);
        if expired {
            self.confirm = None;
            if self.confirm_notice {
                self.notice = None;
                self.confirm_notice = false;
                self.needs_redraw = true;
            }
        }
        // Fade transient notices. Confirm prompts manage their own lifecycle
        // above (they die with the arm, not the clock).
        if !self.confirm_notice
            && self
                .notice_at
                .is_some_and(|t| t.elapsed().as_millis() > NOTICE_TTL_MS)
        {
            self.notice = None;
            self.notice_at = None;
            self.needs_redraw = true;
        }
    }

    fn select_first_block(&mut self) {
        self.transcript.selected = (!self.transcript.cells.is_empty()).then_some(0);
        self.scroll = usize::MAX;
    }

    fn select_last_block(&mut self) {
        self.transcript.selected = self.transcript.cells.len().checked_sub(1);
        self.scroll = 0;
    }
    fn toggle_multiline(&mut self) {
        self.multiline = !self.multiline;
        self.set_notice(if self.multiline {
            "multiline on · Enter newline · Alt+Enter send"
        } else {
            "multiline off · Enter send"
        });
    }
    fn open_palette(&mut self) {
        if !self.catalog_loaded && !self.demo {
            let _ = self.cmd_tx.send(Cmd::FetchCatalog {
                session_id: self.session_id.clone(),
            });
        }
        let row =
            |section: &'static str, action: &str, label: &str, shortcut: Option<&str>| PaletteRow {
                label: label.to_string(),
                action: action.to_string(),
                shortcut: shortcut.map(str::to_string),
                section,
            };
        let mut rows = vec![
            row("Session", "New Session", "/new", Some("Ctrl+N")),
            row("Session", "Resume Session", "/resume", Some("Ctrl+S")),
            row("Session", "Session Info & Context", "/context", None),
            row("Session", "Copy Last Response", "/copy", None),
            row("Session", "Quit", "/exit", Some("Ctrl+Q")),
            row("Context", "Rewind Conversation", "/rewind", Some("2×Esc")),
            row(
                "Model & Input",
                "Toggle Multiline",
                "/multiline",
                Some("Ctrl+M"),
            ),
            row(
                "Model & Input",
                "Prompt History",
                "/history",
                Some("Ctrl+R"),
            ),
            row("Appearance", "Switch Theme", "/theme", None),
            row(
                "Appearance",
                "Mouse Reporting (off to select text)",
                "/mouse",
                None,
            ),
            row("Appearance", "Keyboard Shortcuts", "/help", Some("Ctrl+X")),
            row("Connection", "List Providers", "/provider", None),
            row("Connection", "Add Provider", "/provider add", None),
            row("Panels", "Prompt Queue", "/queue", Some("Ctrl+;")),
            row("Panels", "Todos", "/todos", Some("Ctrl+T")),
        ];
        if !self.catalog_loaded || self.capabilities.models {
            rows.push(row(
                "Model & Input",
                "Switch Model",
                "/model",
                Some("Ctrl+M"),
            ));
        }
        if !self.catalog_loaded || self.capabilities.compaction {
            rows.push(row("Context", "Compact History", "/compact", None));
        }
        if !self.catalog_loaded || self.capabilities.plan_mode {
            rows.push(row("Model & Input", "Enter Plan Mode", "/plan", None));
        }
        if !self.catalog_loaded || self.capabilities.permissions {
            rows.push(row(
                "Model & Input",
                "Toggle Always Approve",
                "/always-approve",
                Some("Shift+Tab"),
            ));
        }
        if !self.catalog_loaded || self.capabilities.jobs {
            rows.push(row("Panels", "Tasks", "/jobs", Some("Ctrl+G")));
        }
        if self.catalog_loaded {
            let enabled = [
                (self.capabilities.user_questions, "questions"),
                (self.capabilities.session_search, "session search"),
                (self.capabilities.commands, "commands"),
                (self.capabilities.tools, "tools"),
            ]
            .into_iter()
            .filter_map(|(present, label)| present.then_some(label))
            .collect::<Vec<_>>()
            .join(", ");
            if !enabled.is_empty() {
                rows.push(row(
                    "Harness",
                    &format!("Available: {enabled}"),
                    "/session-info",
                    None,
                ));
            }
        }
        rows.extend(self.harness_commands.iter().map(|command| PaletteRow {
            label: format!("/{}", command.name),
            action: command.description.clone(),
            shortcut: command.input_hint.clone(),
            section: "Harness",
        }));
        if self.demo {
            rows.extend([
                row("Demo", "Permission Dialog", "demo-approval", Some("F2")),
                row("Demo", "Question Dialog", "demo-question", Some("F3")),
            ]);
        }
        let visible = (0..rows.len()).collect();
        self.dialog = Dialog::Palette(Palette {
            rows,
            filter: String::new(),
            selected: 0,
            visible,
        });
    }

    fn open_shortcuts(&mut self) {
        use ShortcutSection::{Actions, Essentials, Input, Navigation, Panels, Session};
        let rows = vec![
            ShortcutRow {
                label: "Send",
                keys: "Enter",
                section: Essentials,
            },
            ShortcutRow {
                label: "Focus scrollback",
                keys: "Tab",
                section: Essentials,
            },
            ShortcutRow {
                label: "Cancel turn",
                keys: "Ctrl+C / Esc",
                section: Essentials,
            },
            ShortcutRow {
                label: "Cycle mode",
                keys: "Shift+Tab",
                section: Essentials,
            },
            ShortcutRow {
                label: "Quit",
                keys: "Ctrl+Q / Ctrl+C ×2 (Ctrl+D in composer)",
                section: Essentials,
            },
            ShortcutRow {
                label: "Command palette",
                keys: "Ctrl+P / ?",
                section: Essentials,
            },
            ShortcutRow {
                label: "Prompt queue",
                keys: "Ctrl+; / Ctrl+'",
                section: Panels,
            },
            ShortcutRow {
                label: "Send now",
                keys: "Ctrl+Enter / Ctrl+I",
                section: Actions,
            },
            ShortcutRow {
                label: "Prompt history",
                keys: "Ctrl+R / /history",
                section: Input,
            },
            ShortcutRow {
                label: "Keyboard shortcuts",
                keys: "Ctrl+X / Ctrl+.",
                section: Essentials,
            },
            ShortcutRow {
                label: "Toggle multiline",
                keys: "Ctrl+M",
                section: Input,
            },
            ShortcutRow {
                label: "Insert newline / send multiline",
                keys: "Shift+Enter / Alt+Enter",
                section: Input,
            },
            ShortcutRow {
                label: "Mention file",
                keys: "@",
                section: Input,
            },
            ShortcutRow {
                label: "Move caret",
                keys: "← / →",
                section: Input,
            },
            ShortcutRow {
                label: "Move caret by word",
                keys: "Alt+← / Alt+→ / Alt+B / Alt+F",
                section: Input,
            },
            ShortcutRow {
                label: "Start / end of line",
                keys: "Ctrl+A / Ctrl+E / Home / End",
                section: Input,
            },
            ShortcutRow {
                label: "Delete word",
                keys: "Ctrl+W / Alt+Backspace / Alt+D",
                section: Input,
            },
            ShortcutRow {
                label: "Delete to line start / end",
                keys: "Ctrl+U / Ctrl+K",
                section: Input,
            },
            ShortcutRow {
                label: "Undo edit",
                keys: "Ctrl+Z",
                section: Input,
            },
            ShortcutRow {
                label: "Prompt history",
                keys: "↑ / ↓",
                section: Input,
            },
            ShortcutRow {
                label: "Select block",
                keys: "↑ / ↓",
                section: Navigation,
            },
            ShortcutRow {
                label: "Previous / next turn",
                keys: "Shift+H / Shift+L",
                section: Navigation,
            },
            ShortcutRow {
                label: "Previous / next reply",
                keys: "Shift+K / Shift+J",
                section: Navigation,
            },
            ShortcutRow {
                label: "Scroll viewport (any focus)",
                keys: "Shift+↑ / Shift+↓ · PageUp / PageDown",
                section: Navigation,
            },
            ShortcutRow {
                label: "Scroll one line",
                keys: "Ctrl+K / Ctrl+J",
                section: Navigation,
            },
            ShortcutRow {
                label: "Scroll half page",
                keys: "Ctrl+U / Ctrl+D",
                section: Navigation,
            },
            ShortcutRow {
                label: "Collapse / expand block",
                keys: "← / → / h / l / e",
                section: Navigation,
            },
            ShortcutRow {
                label: "Collapse / expand all",
                keys: "Shift+E",
                section: Navigation,
            },
            ShortcutRow {
                label: "Open block fullscreen",
                keys: "Enter / Ctrl+F",
                section: Actions,
            },
            ShortcutRow {
                label: "Always-approve",
                keys: "Ctrl+O / Shift+Tab",
                section: Actions,
            },
            ShortcutRow {
                label: "Jump to top / bottom",
                keys: "g / G / Home / End",
                section: Navigation,
            },
            ShortcutRow {
                label: "Page conversation",
                keys: "PageUp / PageDown",
                section: Navigation,
            },
            ShortcutRow {
                label: "Fold all thoughts",
                keys: "Ctrl+E (scrollback)",
                section: Actions,
            },
            ShortcutRow {
                label: "Select / copy text with the mouse",
                keys: "works by default (mouse reporting is off)",
                section: Actions,
            },
            ShortcutRow {
                label: "Mouse wheel scrolling",
                keys: "/mouse to enable (then Shift to select)",
                section: Actions,
            },
            ShortcutRow {
                label: "Copy selected block",
                keys: "y",
                section: Actions,
            },
            ShortcutRow {
                label: "Rewind conversation",
                keys: "2×Esc",
                section: Actions,
            },
            ShortcutRow {
                label: "Model selector",
                keys: "Ctrl+M",
                section: Panels,
            },
            ShortcutRow {
                label: "Tasks",
                keys: "Ctrl+G",
                section: Panels,
            },
            ShortcutRow {
                label: "Todos",
                keys: "Ctrl+T",
                section: Panels,
            },
            ShortcutRow {
                label: "Theme",
                keys: "/theme",
                section: Panels,
            },
            ShortcutRow {
                label: "New session",
                keys: "Ctrl+N ×2",
                section: Session,
            },
            ShortcutRow {
                label: "Resume session",
                keys: "Ctrl+S",
                section: Session,
            },
        ];
        self.dialog = Dialog::Shortcuts(Shortcuts {
            rows,
            filter: String::new(),
            selected_section: 0,
            expanded: [true, false, false, false, false, false],
        });
    }

    fn cycle_permission_mode(&mut self) {
        self.permission_mode = match self.permission_mode {
            PermissionMode::Normal => PermissionMode::Plan,
            PermissionMode::Plan => PermissionMode::AlwaysApprove,
            PermissionMode::AlwaysApprove => PermissionMode::Normal,
        };
        self.set_notice(format!("mode: {}", self.permission_mode.label()));
        self.apply_permission_mode();
    }

    fn apply_permission_mode(&mut self) {
        if self.permission_presets.is_empty() {
            self.catalog_for_presets = true;
            let _ = self.cmd_tx.send(Cmd::FetchCatalog {
                session_id: self.session_id.clone(),
            });
            return;
        }
        let named = |name: &str| {
            self.permission_presets
                .iter()
                .position(|preset| preset == name)
        };
        let target = match self.permission_mode {
            PermissionMode::Normal => named("workspace-write").or_else(|| {
                self.permission_presets
                    .iter()
                    .position(|preset| !preset.contains("danger") && !preset.contains("approve"))
            }),
            PermissionMode::Plan => named("read-only")
                .or_else(|| named("workspace-write"))
                .or_else(|| {
                    self.permission_presets.iter().position(|preset| {
                        !preset.contains("danger") && !preset.contains("approve")
                    })
                }),
            PermissionMode::AlwaysApprove => named("danger-full-access").or_else(|| {
                self.permission_presets
                    .iter()
                    .position(|preset| preset.contains("danger") || preset.contains("approve"))
            }),
        }
        .unwrap_or(self.preset_index);
        self.preset_index = target;
        let _ = self.cmd_tx.send(Cmd::SetMode {
            session_id: self.session_id.clone(),
            plan: self.permission_mode == PermissionMode::Plan,
            preset: self.permission_presets[target].clone(),
        });
    }

    fn open_rewind(&mut self) {
        let entries: Vec<RewindEntry> = self
            .transcript
            .turns
            .iter()
            .map(|t| {
                let preview = self
                    .transcript
                    .cells
                    .get(t.cell)
                    .map(|c| {
                        let one: String = c.text.replace('\n', " ").chars().take(48).collect();
                        one
                    })
                    .unwrap_or_default();
                RewindEntry {
                    seq: t.seq,
                    preview,
                }
            })
            .collect();
        if entries.is_empty() {
            self.set_notice("no turns to rewind");
        } else {
            self.dialog = Dialog::Rewind(RewindPicker {
                items: entries,
                selected: 0,
            });
        }
    }

    fn copy_text(&mut self, text: String) {
        if text.is_empty() {
            self.set_notice("nothing to copy");
            return;
        }
        let out = clipboard::copy(&text);
        if out.delivered {
            self.set_notice("copied");
        } else {
            self.set_notice(format!(
                "clipboard unreachable; saved to {}",
                out.backup.display()
            ));
        }
    }

    fn run_command(&mut self, cmd: &str) {
        let mut parts = cmd.split_whitespace();
        let name = parts.next().unwrap_or("");
        match name {
            "/resume" => {
                let _ = self.cmd_tx.send(Cmd::ListLive);
                let mut items = list_sessions(&self.workspace, &self.session_id);
                items.retain(|it| !self.live_ids.contains(&it.id));
                if items.is_empty() {
                    self.set_notice("no sessions found");
                } else {
                    self.dialog = Dialog::Resume(ResumePicker { items, selected: 0 });
                }
            }
            "/new" | "/clear" => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis();
                self.session_id = format!("dsh-{now}");
                self.transcript = Transcript::new();
                self.queue.clear();
                self.scroll = 0;
                self.status = "new session".into();
            }
            "/exit" | "/quit" => self.quit = true,
            "/copy" => {
                let text = self
                    .transcript
                    .cells
                    .iter()
                    .rev()
                    .find(|c| c.kind == CellKind::Assistant)
                    .map(|c| c.text.clone())
                    .unwrap_or_default();
                self.copy_text(text);
            }
            "/session-info" | "/context" | "/status" | "/info" => {
                if self.demo {
                    // No bridge in demo mode, so answer locally — the demo is
                    // meant to exercise every surface without a runtime.
                    self.handle(AppEvent::Rpc {
                        method: "tui/session-info-result".into(),
                        params: json!({
                            "sessionId": self.session_id,
                            "provider": self.provider,
                            "model": self.model,
                            "cwd": self.workspace,
                            "turns": self.transcript.stats.turns,
                        }),
                    });
                    return;
                }
                let _ = self.cmd_tx.send(Cmd::SessionInfo {
                    session_id: self.session_id.clone(),
                });
            }
            "/mouse" => {
                self.mouse_capture = !self.mouse_capture;
                self.mouse_capture_dirty = true;
                self.set_notice(if self.mouse_capture {
                    "mouse on · wheel scrolls · text selection needs Shift now"
                } else {
                    "mouse off · select text natively · wheel falls back to arrow keys"
                });
            }
            "/theme" => {
                if self.has_dialog() {
                    self.dialog = Dialog::None;
                } else {
                    let original = self.theme;
                    let selected = if self.theme.name == "dark" { 0 } else { 1 };
                    self.dialog = Dialog::Theme(ThemePicker { original, selected });
                }
            }
            "/help" | "/shortcuts" => self.open_shortcuts(),
            "/model" | "/m" => {
                self.catalog_for_picker = true;
                self.status = "loading models".into();
                let _ = self.cmd_tx.send(Cmd::FetchCatalog {
                    session_id: self.session_id.clone(),
                });
            }
            "/compact" => {
                self.status = "compacting".into();
                let _ = self.cmd_tx.send(Cmd::Compact {
                    session_id: self.session_id.clone(),
                });
            }
            "/rewind" | "/undo" => self.open_rewind(),
            "/multiline" | "/ml" => self.toggle_multiline(),
            "/history" => self.open_history(),
            "/queue" => self.open_queue(),
            "/todos" | "/todo" => self.open_todos(),
            "/jobs" | "/tasks" => {
                self.status = "loading tasks".into();
                let _ = self.cmd_tx.send(Cmd::FetchJobs);
            }
            "/plan" => {
                self.permission_mode = PermissionMode::Plan;
                self.set_notice("mode: Plan");
                self.apply_permission_mode();
            }
            "/always-approve" => {
                self.permission_mode = if self.permission_mode == PermissionMode::AlwaysApprove {
                    PermissionMode::Normal
                } else {
                    PermissionMode::AlwaysApprove
                };
                self.apply_permission_mode();
            }
            "/provider" | "/providers" => match parts.next() {
                Some("add") => {
                    self.dialog = Dialog::Provider(ProviderWizard::new(
                        self.provider_protocols.clone(),
                        self.catalog_providers.clone(),
                    ));
                    // 目录预设还没拉过时后台补一次（tui/providers 会回填）。
                    if self.catalog_providers.is_empty() && !self.demo {
                        let _ = self.cmd_tx.send(Cmd::ListProviders);
                    }
                }
                Some(other) => {
                    self.set_notice(format!("unknown /provider subcommand: {other} (try add)"));
                }
                None => {
                    if self.demo {
                        self.set_notice("demo: providers come from the live bridge");
                    } else {
                        self.status = "loading providers".into();
                        let _ = self.cmd_tx.send(Cmd::ListProviders);
                    }
                }
            },
            other if other.starts_with('/') => {
                let _ = self.cmd_tx.send(Cmd::ExecuteCommand {
                    session_id: self.session_id.clone(),
                    line: cmd.to_string(),
                });
                self.status = format!("running Harness command {other}");
            }
            _ => self.set_notice(format!("unknown command: {name}")),
        }
    }

    /// Dialog::Provider 的 Enter：按步骤分发——Type 定分支、Known 选目录
    /// 项、文本步校验步进、Models 展开行、Confirm 发桥端保存（面板保持
    /// 打开，结果回填 error 或关闭）。
    fn provider_wizard_enter(&mut self) {
        let Dialog::Provider(wizard) = &mut self.dialog else {
            return;
        };
        if wizard.saving {
            return;
        }
        match wizard.step {
            ProviderStep::Type => {
                wizard.kind = if wizard.type_sel == 0 {
                    Some(WizardKind::Known)
                } else {
                    Some(WizardKind::Custom)
                };
                if wizard.kind == Some(WizardKind::Known) && wizard.catalog.is_empty() {
                    wizard.error = Some("catalog not loaded yet — retry in a moment".into());
                    return;
                }
                wizard.advance();
            }
            ProviderStep::Known => {
                let Some(preset) = wizard.catalog.get(wizard.catalog_sel) else {
                    return;
                };
                wizard.id = preset.id.clone();
                wizard.advance();
            }
            ProviderStep::Api if !wizard.protocols.is_empty() => {
                wizard.api = wizard.protocols[wizard.proto_sel].clone();
                wizard.advance();
            }
            ProviderStep::Models => match wizard.models_focus {
                ModelsFocus::List => {
                    if let Some(model) = wizard.models.get_mut(wizard.model_cursor) {
                        model.open = !model.open;
                        wizard.models_focus = if model.open {
                            wizard.detail_col = 0;
                            ModelsFocus::Detail
                        } else {
                            ModelsFocus::List
                        };
                    }
                }
                ModelsFocus::Manual => {
                    let id = wizard.manual.trim().to_string();
                    if !id.is_empty() && !wizard.models.iter().any(|m| m.id == id) {
                        wizard.models.push(ModelDraft {
                            id,
                            included: true,
                            vision: false,
                            reasoning: false,
                            efforts: Vec::new(),
                            open: false,
                        });
                        wizard.model_cursor = wizard.models.len() - 1;
                    }
                    wizard.manual.clear();
                    wizard.models_focus = ModelsFocus::List;
                }
                ModelsFocus::Detail => wizard_toggle_detail(wizard),
            },
            ProviderStep::Confirm => {
                // 自定义路由至少一个模型（宿主 resolveRouteModels 对目录外
                // 路由无 models 直接 invalid，提前拦截友好于吃 schema 错误）。
                if wizard.kind == Some(WizardKind::Custom)
                    && !wizard.models.iter().any(|m| m.included)
                {
                    wizard.error =
                        Some("custom routes must declare at least one model".to_string());
                    return;
                }
                if self.demo {
                    wizard.error = Some("demo: provider writes need the live bridge".into());
                    return;
                }
                wizard.saving = true;
                wizard.error = None;
                let draft = wizard.save_payload();
                let _ = self.cmd_tx.send(Cmd::SaveProvider { draft });
            }
            _ => {
                if let Err(message) = wizard.commit_draft() {
                    wizard.error = Some(message);
                    return;
                }
                wizard.advance();
            }
        }
    }

    /// key/删除子面板取消或完成后回到列表：重发请求，tui/providers 会重开面板。
    fn reopen_provider_list(&mut self) {
        self.dialog = Dialog::None;
        if !self.demo {
            self.status = "loading providers".into();
            let _ = self.cmd_tx.send(Cmd::ListProviders);
        }
    }

    fn cancel_now(&mut self) {
        self.esc = EscArm::None;
        // docs/01 section 2.5: for ~1s after an Esc-triggered cancel the idle
        // rewind arm stays suppressed, so mashing Esc cannot open the picker.
        self.cancel_grace = Some(Instant::now());
        if self.demo {
            self.state = RunState::Idle;
            self.status = "cancelled (demo)".into();
            return;
        }
        let _ = self.cmd_tx.send(Cmd::Cancel {
            session_id: self.session_id.clone(),
        });
        self.status = "cancelling".into();
    }

    pub fn handle(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::Rpc { method, params } => match method.as_str() {
                "session.event" => {
                    let sid = params
                        .get("sessionId")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if let Some(event) = params.get("event") {
                        if sid == self.session_id {
                            self.transcript.apply(event);
                        } else {
                            let t = self.child_transcripts.entry(sid.to_string()).or_default();
                            t.apply(event);
                        }
                    }
                }
                "session.projection" => {
                    // One key-agnostic channel for every projection, present and
                    // future (docs/04 section 3.3): a new harness projection
                    // needs no protocol change.
                    let sid = params
                        .get("sessionId")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if sid == self.session_id {
                        if let Some(key) = params.get("key").and_then(|v| v.as_str()) {
                            let value = params.get("value").unwrap_or(&Value::Null);
                            let seq = params.get("seq").and_then(|v| v.as_u64()).unwrap_or(0);
                            self.projections.apply(key, value, seq);
                            self.sync_todos_from_projection();
                        }
                    }
                }
                "subagent.started" => {
                    let parent = params
                        .get("parentSessionId")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let child = params
                        .get("childSessionId")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if parent == self.session_id {
                        let i = self.transcript.push(
                            CellKind::Subagent,
                            "subagent".to_string(),
                            format!("started {child}"),
                        );
                        self.transcript.cells[i].link = Some(child.to_string());
                        self.transcript.selected = Some(i);
                        self.child_transcripts.entry(child.to_string()).or_default();
                    }
                }
                "subagent.finished" => {
                    let child = params
                        .get("childSessionId")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if let Some(i) = self
                        .transcript
                        .cells
                        .iter()
                        .position(|c| c.link.as_deref() == Some(child))
                    {
                        let status = params
                            .get("status")
                            .and_then(|v| v.as_str())
                            .unwrap_or("done");
                        let text = params
                            .get("lastAssistantMessage")
                            .and_then(Value::as_array)
                            .and_then(|blocks| content_text(blocks))
                            .unwrap_or_default();
                        self.transcript.cells[i].text = if text.is_empty() {
                            status.to_string()
                        } else {
                            format!("{status}: {text}")
                        };
                        self.transcript.selected = Some(i);
                    }
                }
                "session.status" => {
                    let sid = params
                        .get("sessionId")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if sid == self.session_id {
                        let running =
                            params.get("status").and_then(Value::as_str) == Some("running");
                        self.state = if running {
                            RunState::Running
                        } else {
                            RunState::Idle
                        };
                        self.status = if running {
                            "running".into()
                        } else {
                            "idle".into()
                        };
                    }
                }
                "session.queue" => self.replace_queue(&params),
                "tui/ready" => {
                    if let Some(current) = params
                        .get("server")
                        .and_then(|server| server.get("current"))
                    {
                        if let Some(provider) = current.get("provider").and_then(Value::as_str) {
                            self.provider = provider.to_string();
                        }
                        if let Some(model) = current.get("model").and_then(Value::as_str) {
                            self.model = model.to_string();
                        }
                        self.reasoning_effort = current
                            .get("reasoningEffort")
                            .and_then(Value::as_str)
                            .map(String::from);
                        self.reasoning_effort_name = self.reasoning_effort.clone();
                    }
                    if let Some(protocols) = params
                        .get("server")
                        .and_then(|server| server.get("protocols"))
                        .and_then(Value::as_array)
                    {
                        self.provider_protocols = protocols
                            .iter()
                            .filter_map(|value| value.as_str().map(String::from))
                            .collect();
                        // 向导开着时同步词表（/provider add 先于 ready 完成时）。
                        if let Dialog::Provider(wizard) = &mut self.dialog {
                            wizard.protocols = self.provider_protocols.clone();
                        }
                    }
                    self.status = "runtime ready".into();
                    self.state = RunState::Idle;
                    if !self.demo {
                        let _ = self.cmd_tx.send(Cmd::FetchCatalog {
                            session_id: self.session_id.clone(),
                        });
                    }
                }
                "tui/provider-saved" => {
                    self.status = if self.is_running() {
                        "running".into()
                    } else {
                        "idle".into()
                    };
                    let ok = params.get("ok").and_then(Value::as_bool).unwrap_or(false);
                    let error = params
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error");
                    let mut close_dialog = false;
                    if let Dialog::Provider(wizard) = &mut self.dialog {
                        wizard.saving = false;
                        if ok {
                            close_dialog = true;
                        } else {
                            // 失败留在面板里：字段还在，改完 Enter 重试。
                            wizard.error = Some(error.to_string());
                        }
                    }
                    if ok {
                        if close_dialog {
                            self.dialog = Dialog::None;
                        }
                        let id = params.get("id").and_then(Value::as_str).unwrap_or("?");
                        let key_note =
                            if params.get("keyStored").and_then(Value::as_bool) == Some(true) {
                                "key stored".to_string()
                            } else if let Some(key_error) =
                                params.get("keyError").and_then(Value::as_str)
                            {
                                format!("key not stored: {key_error}")
                            } else {
                                "no key given".to_string()
                            };
                        self.set_notice(format!(
                            "provider {id} added · {key_note} · pick it via /model"
                        ));
                        // 目录缓存里还没有新服务商，后台刷新让 /model 立即可选。
                        if !self.demo {
                            let _ = self.cmd_tx.send(Cmd::FetchCatalog {
                                session_id: self.session_id.clone(),
                            });
                        }
                    } else if !matches!(self.dialog, Dialog::Provider(_)) {
                        self.set_notice(format!("add provider failed: {error}"));
                    }
                }
                "tui/providers" => {
                    if let Some(protocols) = params.get("protocols").and_then(Value::as_array) {
                        self.provider_protocols = protocols
                            .iter()
                            .filter_map(|value| value.as_str().map(String::from))
                            .collect();
                    }
                    self.status = if self.is_running() {
                        "running".into()
                    } else {
                        "idle".into()
                    };
                    let items: Vec<ProviderItem> = params
                        .get("providers")
                        .and_then(Value::as_array)
                        .map(|list| {
                            list.iter()
                                .map(|p| {
                                    let id = p
                                        .get("id")
                                        .and_then(Value::as_str)
                                        .unwrap_or("?")
                                        .to_string();
                                    let key = p
                                        .get("key")
                                        .and_then(|k| {
                                            k.get("configured")
                                                .and_then(Value::as_bool)
                                                .filter(|configured| *configured)
                                        })
                                        .and_then(|_| {
                                            p.get("key")
                                                .and_then(|k| k.get("source"))
                                                .and_then(Value::as_str)
                                        })
                                        .unwrap_or("none")
                                        .to_string();
                                    ProviderItem { id, key }
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    if let Some(catalog) = params.get("catalogProviders").and_then(Value::as_array)
                    {
                        self.catalog_providers = catalog
                            .iter()
                            .filter_map(|p| {
                                Some(CatalogProvider {
                                    id: p.get("id").and_then(Value::as_str)?.to_string(),
                                    name: p
                                        .get("name")
                                        .and_then(Value::as_str)
                                        .unwrap_or("?")
                                        .to_string(),
                                    key_page: p
                                        .get("keyPage")
                                        .and_then(Value::as_str)
                                        .map(String::from),
                                })
                            })
                            .collect();
                        // 向导开着时回填它的目录选项，不要切走面板。
                        if let Dialog::Provider(wizard) = &mut self.dialog {
                            wizard.catalog = self.catalog_providers.clone();
                        }
                    }
                    if items.is_empty() {
                        if !matches!(self.dialog, Dialog::Provider(_)) {
                            self.set_notice("no providers configured · /provider add");
                        }
                    } else if !matches!(self.dialog, Dialog::Provider(_)) {
                        // 向导/子面板开着时不抢面板（add 流程里的后台刷新）。
                        self.dialog = Dialog::ProviderList(ProviderListView { items, selected: 0 });
                    }
                }
                "tui/models-fetched" => {
                    let ok = params.get("ok").and_then(Value::as_bool).unwrap_or(false);
                    if let Dialog::Provider(wizard) = &mut self.dialog {
                        wizard.fetching = false;
                        if ok {
                            let mut added = 0usize;
                            for id in params
                                .get("models")
                                .and_then(Value::as_array)
                                .map(|list| {
                                    list.iter().filter_map(Value::as_str).collect::<Vec<_>>()
                                })
                                .unwrap_or_default()
                            {
                                if !wizard.models.iter().any(|m| m.id == id) {
                                    wizard.models.push(ModelDraft {
                                        id: id.to_string(),
                                        included: true,
                                        vision: false,
                                        reasoning: true,
                                        efforts: vec![
                                            "low".into(),
                                            "medium".into(),
                                            "high".into(),
                                            "xhigh".into(),
                                            "max".into(),
                                        ],
                                        open: false,
                                    });
                                    added += 1;
                                }
                            }
                            if added == 0 {
                                wizard.error = Some("endpoint listed no new models".into());
                            }
                        } else {
                            wizard.error = Some(
                                params
                                    .get("error")
                                    .and_then(Value::as_str)
                                    .unwrap_or("fetch failed")
                                    .to_string(),
                            );
                        }
                    }
                }
                "tui/provider-removed" => {
                    self.status = if self.is_running() {
                        "running".into()
                    } else {
                        "idle".into()
                    };
                    let ok = params.get("ok").and_then(Value::as_bool).unwrap_or(false);
                    let id = params.get("id").and_then(Value::as_str).unwrap_or("?");
                    if ok {
                        self.set_notice(format!("provider {id} removed"));
                        if !self.demo {
                            let _ = self.cmd_tx.send(Cmd::FetchCatalog {
                                session_id: self.session_id.clone(),
                            });
                        }
                    } else {
                        let error = params
                            .get("error")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown error");
                        self.set_notice(format!("remove {id} failed: {error}"));
                    }
                    self.reopen_provider_list();
                }
                "tui/key-saved" => {
                    let ok = params.get("ok").and_then(Value::as_bool).unwrap_or(false);
                    if let Dialog::ProviderKey(dialog) = &mut self.dialog {
                        dialog.saving = false;
                        if !ok {
                            dialog.error = Some(
                                params
                                    .get("error")
                                    .and_then(Value::as_str)
                                    .unwrap_or("unknown error")
                                    .to_string(),
                            );
                            return;
                        }
                    }
                    if ok {
                        let id = params.get("id").and_then(Value::as_str).unwrap_or("?");
                        self.set_notice(format!("key stored for {id}"));
                        self.reopen_provider_list();
                    } else if !matches!(self.dialog, Dialog::ProviderKey(_)) {
                        let error = params
                            .get("error")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown error");
                        self.set_notice(format!("set key failed: {error}"));
                    }
                }
                "tui/catalog-result" => {
                    if let Some(names) = params.get("permissionPresets").and_then(Value::as_array) {
                        self.permission_presets = names
                            .iter()
                            .filter_map(|value| value.as_str().map(String::from))
                            .collect();
                    }
                    let current = params.get("current");
                    let current_provider = current
                        .and_then(|value| value.get("provider"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let capabilities = params.get("capabilities");
                    self.capabilities = HarnessCapabilities {
                        models: capability_flag(capabilities, "models"),
                        permissions: capability_flag(capabilities, "permissions"),
                        plan_mode: capability_flag(capabilities, "planMode"),
                        compaction: capability_flag(capabilities, "compaction"),
                        jobs: capability_flag(capabilities, "jobs"),
                        user_questions: capability_flag(capabilities, "userQuestions"),
                        session_search: capability_flag(capabilities, "sessionSearch"),
                        commands: capability_flag(capabilities, "commands"),
                        tools: capability_flag(capabilities, "tools"),
                        projections: capability_flag(capabilities, "projections"),
                        goals: capability_flag(capabilities, "goals"),
                    };
                    self.harness_commands = params
                        .get("commands")
                        .and_then(Value::as_array)
                        .map(|commands| {
                            commands
                                .iter()
                                .filter_map(|command| {
                                    Some(HarnessCommand {
                                        name: command.get("name")?.as_str()?.to_string(),
                                        description: command
                                            .get("description")
                                            .and_then(Value::as_str)
                                            .unwrap_or("")
                                            .to_string(),
                                        input_hint: command
                                            .get("inputHint")
                                            .and_then(Value::as_str)
                                            .map(String::from),
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    self.catalog_loaded = true;
                    if matches!(self.dialog, Dialog::Palette(_)) {
                        self.open_palette();
                    }
                    let current_model = current
                        .and_then(|value| value.get("model"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let current_effort = current
                        .and_then(|value| value.get("reasoningEffort"))
                        .and_then(Value::as_str)
                        .map(String::from);
                    let mut rows: Vec<ModelEntry> = params
                        .get("models")
                        .and_then(Value::as_array)
                        .map(|models| {
                            models
                                .iter()
                                .filter_map(|model| {
                                    let reasoning = model
                                        .get("reasoning")
                                        .filter(|value| !value.is_null())
                                        .map(|value| ModelReasoning {
                                            efforts: value
                                                .get("efforts")
                                                .and_then(Value::as_array)
                                                .map(|efforts| {
                                                    efforts
                                                        .iter()
                                                        .filter_map(|effort| {
                                                            Some(ReasoningEffortEntry {
                                                                id: effort
                                                                    .get("id")?
                                                                    .as_str()?
                                                                    .to_string(),
                                                                name: effort
                                                                    .get("name")
                                                                    .and_then(Value::as_str)
                                                                    .unwrap_or("")
                                                                    .to_string(),
                                                                description: effort
                                                                    .get("description")
                                                                    .and_then(Value::as_str)
                                                                    .map(String::from),
                                                            })
                                                        })
                                                        .collect()
                                                })
                                                .unwrap_or_default(),
                                            default_effort: value
                                                .get("defaultEffort")
                                                .and_then(Value::as_str)
                                                .map(String::from),
                                        });
                                    Some(ModelEntry {
                                        provider: model.get("provider")?.as_str()?.to_string(),
                                        id: model.get("id")?.as_str()?.to_string(),
                                        name: model
                                            .get("name")
                                            .and_then(Value::as_str)
                                            .unwrap_or("")
                                            .to_string(),
                                        description: model
                                            .get("description")
                                            .and_then(Value::as_str)
                                            .map(String::from),
                                        context_window: model
                                            .get("contextWindow")
                                            .and_then(Value::as_u64),
                                        reasoning,
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    rows.sort_by_key(|row| {
                        let current_provider = row.provider == current_provider;
                        let exact = current_provider && row.id == current_model;
                        (
                            if current_provider { 0 } else { 1 },
                            if exact { 0 } else { 1 },
                            row.id.clone(),
                        )
                    });
                    let selected = rows
                        .iter()
                        .position(|row| row.provider == current_provider && row.id == current_model)
                        .unwrap_or(0);
                    if let Some(row) = rows.get(selected) {
                        self.provider = current_provider.clone();
                        self.model = current_model.clone();
                        self.reasoning_effort = current_effort.or_else(|| {
                            row.reasoning
                                .as_ref()
                                .and_then(|reasoning| reasoning.default_effort.clone())
                        });
                        self.reasoning_effort_name = row.reasoning.as_ref().and_then(|reasoning| {
                            let current = self.reasoning_effort.as_deref()?;
                            reasoning
                                .efforts
                                .iter()
                                .find(|effort| effort.id == current)
                                .map(|effort| effort.name.clone())
                        });
                        self.context_window = row.context_window;
                    }
                    let for_presets = self.catalog_for_presets;
                    let for_picker = self.catalog_for_picker;
                    self.catalog_for_presets = false;
                    self.catalog_for_picker = false;
                    if for_presets {
                        self.set_notice(format!(
                            "{} presets loaded",
                            self.permission_presets.len()
                        ));
                        self.apply_permission_mode();
                    } else if for_picker {
                        if rows.is_empty() {
                            self.set_notice("catalog empty");
                        } else {
                            self.dialog = Dialog::Model(ModelPicker {
                                rows,
                                filter: String::new(),
                                selected,
                            });
                        }
                    }
                    // 其余来源（tui/ready 启动拉取、capabilities-changed 后台
                    // 刷新）只更新 capabilities/presets，不弹任何对话框。
                }
                "tui/model-set" => {
                    if let Some(current) = params.get("current") {
                        let provider = current
                            .get("provider")
                            .and_then(Value::as_str)
                            .unwrap_or("?");
                        let model = current.get("model").and_then(Value::as_str).unwrap_or("?");
                        self.provider = provider.to_string();
                        self.model = model.to_string();
                        self.reasoning_effort = current
                            .get("reasoningEffort")
                            .and_then(Value::as_str)
                            .map(String::from);
                        self.reasoning_effort_name = self.reasoning_effort.clone();
                        self.status = format!("model: {provider}/{model}");
                        // 记住选择：下次启动 local defaults 直接命中（settings.rs）。
                        if let Err(e) =
                            crate::settings::update(&[("provider", provider), ("model", model)])
                        {
                            self.set_notice(format!("settings save failed: {e}"));
                        }
                    }
                }
                "tui.capabilities-changed" => {
                    let _ = self.cmd_tx.send(Cmd::FetchCatalog {
                        session_id: self.session_id.clone(),
                    });
                }
                "tui/command-result" => {
                    let kind = params
                        .get("kind")
                        .and_then(Value::as_str)
                        .unwrap_or("success");
                    let text = params.get("text").and_then(Value::as_str).unwrap_or("");
                    self.set_notice(if text.is_empty() {
                        format!("Harness command {kind}")
                    } else {
                        format!("{kind}: {text}")
                    });
                }
                "tui/jobs-result" => {
                    let mut rows: Vec<TaskRow> = params
                        .get("jobs")
                        .and_then(|v| v.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|j| {
                                    Some(TaskRow {
                                        id: j.get("id")?.as_str()?.to_string(),
                                        kind: j.get("kind")?.as_str()?.to_string(),
                                        label: j
                                            .get("label")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string(),
                                        status: j
                                            .get("status")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string(),
                                        detail: j
                                            .get("detail")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string(),
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    // 活跃子代理也列进来（与后台任务同视图）
                    for (child, t) in &self.child_transcripts {
                        if !rows.iter().any(|r| r.id == *child) {
                            let turns = t.cells.iter().filter(|c| c.kind == CellKind::User).count();
                            rows.push(TaskRow {
                                id: child.clone(),
                                kind: "subagent".to_string(),
                                label: format!("live child ({} msgs)", turns),
                                status: "running".to_string(),
                                detail: String::new(),
                            });
                        }
                    }
                    self.dialog = Dialog::Tasks(TasksView { rows, selected: 0 });
                }
                "tui/session-info-result" => {
                    let get = |k: &str| {
                        params
                            .get(k)
                            .map(|v| match v {
                                serde_json::Value::String(sv) => sv.clone(),
                                serde_json::Value::Number(n) => n.to_string(),
                                serde_json::Value::Null => "-".to_string(),
                                other => other.to_string(),
                            })
                            .unwrap_or_else(|| "-".to_string())
                    };
                    let stats = self.transcript.stats;
                    let usage = self.transcript.usage;
                    let billed_input = usage
                        .input
                        .saturating_add(usage.cache)
                        .saturating_add(usage.cache_write);
                    let mut rows = vec![
                        ("session".to_string(), get("sessionId")),
                        (
                            "model".to_string(),
                            format!("{}/{}", get("provider"), get("model")),
                        ),
                        ("cwd".to_string(), get("cwd")),
                        (
                            "turns / steps".to_string(),
                            format!("{} / {}", stats.turns, stats.steps),
                        ),
                        (
                            "LLM / tools".to_string(),
                            format!("{} ms / {} ms", stats.llm_ms, stats.tool_ms),
                        ),
                        (
                            "TTFB / TPS".to_string(),
                            format!(
                                "{} ms / {:.1} tok/s",
                                stats.ttft_ms.checked_div(stats.ttft_steps).unwrap_or(0),
                                if stats.decode_ms == 0 {
                                    0.0
                                } else {
                                    stats.decode_tokens as f64 / (stats.decode_ms as f64 / 1000.0)
                                }
                            ),
                        ),
                        (
                            "cache hit".to_string(),
                            format!(
                                "{}%",
                                usage
                                    .cache
                                    .saturating_mul(100)
                                    .checked_div(billed_input)
                                    .unwrap_or(0)
                            ),
                        ),
                        (
                            "tokens".to_string(),
                            format!("in {} · out {}", billed_input, usage.output),
                        ),
                    ];
                    if stats.steps == 0 {
                        rows[3].1 = get("turns");
                    }
                    rows.extend(self.context_rows());
                    self.dialog = Dialog::Info(InfoDialog { rows });
                }
                "tui/rewound" => {
                    if let Some(new_id) = params.get("newSessionId").and_then(|v| v.as_str()) {
                        self.session_id = new_id.to_string();
                    }
                    if let Some(b) = params.get("boundary").and_then(|v| v.as_u64()) {
                        if let Some(t) = self.transcript.turns.iter().find(|t| t.seq == b) {
                            let cut = t.cell + 1;
                            self.transcript.cells.truncate(cut);
                            self.transcript.turns.retain(|m| m.seq <= b);
                            self.transcript.selected = None;
                        }
                    }
                    self.queue.clear();
                    self.scroll = 0;
                    self.status = "rewound".into();
                    self.set_notice("rewound (new session continues here)");
                }
                "tui/compacted" => {
                    self.status = "compacted".into();
                    self.set_notice("history compacted");
                }
                "tui/mode-set" => {
                    if let Some(preset) = params.get("applied").and_then(Value::as_str) {
                        if let Some(index) = self
                            .permission_presets
                            .iter()
                            .position(|candidate| candidate == preset)
                        {
                            self.preset_index = index;
                        }
                        let suffix = params
                            .get("planOutcome")
                            .and_then(Value::as_str)
                            .map(|outcome| format!(" · plan {outcome}"))
                            .unwrap_or_default();
                        self.status = format!("mode: {}{suffix}", self.permission_mode.label());
                    }
                }
                "tui/live-list" => {
                    if let Some(ids) = params.get("ids").and_then(|v| v.as_array()) {
                        self.live_ids = ids
                            .iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect();
                    }
                    if let Dialog::Resume(p) = &mut self.dialog {
                        p.items.retain(|it| !self.live_ids.contains(&it.id));
                        if !p.items.is_empty() && p.selected >= p.items.len() {
                            p.selected = p.items.len() - 1;
                        }
                    }
                }
                "tui/loaded" => {
                    if let Some(sid) = params.get("sessionId").and_then(|v| v.as_str()) {
                        self.session_id = sid.to_string();
                    }
                    self.status = "resumed".into();
                    if let Some(file) = self.pending_resume_file.take() {
                        if let Ok(events) = read_session_events(&file) {
                            self.transcript = Transcript::new();
                            for ev in &events {
                                self.transcript.apply(ev);
                            }
                            self.scroll = 0;
                            self.set_notice(format!("replayed {} events", events.len()));
                        }
                    }
                }
                _ => {}
            },
            AppEvent::ServerRequest { id, method, params } => {
                self.open_dialog(id, &method, &params);
            }
            AppEvent::Term(ev) => {
                // A mouse event that changed nothing must not force a frame.
                // Wheel and click still do; bare movement does not.
                if let Event::Mouse(mouse) = ev {
                    if self.handle_mouse(mouse) {
                        self.needs_redraw = true;
                    }
                    return;
                }
                self.handle_key(ev);
            }
            AppEvent::RuntimeStderr(line) => {
                self.set_notice(line);
            }
            AppEvent::RuntimeExited(code) => {
                if !self.quit {
                    self.set_notice(format!("runtime exited: {:?}", code));
                    self.state = RunState::Idle;
                }
            }
        }
        self.needs_redraw = true;
    }

    fn open_dialog(&mut self, id: String, method: &str, params: &Value) {
        match method {
            "ui/approve" => {
                let tool_name = params
                    .get("toolName")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string();
                let reason = params
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let input = params
                    .get("input")
                    .filter(|v| !v.is_null())
                    .map(|v| v.to_string())
                    .unwrap_or_default();
                let call_id = params
                    .get("callId")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let options = params
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_else(|| vec!["allowed-once".into(), "rejected".into()]);
                // Busy UI fails closed rather than dropping the request.
                if self.has_dialog() {
                    self.respond(id, json!({ "outcome": "unavailable" }));
                    return;
                }
                self.dialog = Dialog::Approval(ApprovalDialog {
                    request_id: id,
                    tool_name,
                    reason,
                    input,
                    call_id,
                    options,
                    selected: 0,
                    parked: false,
                });
            }
            "ui/ask-user" => {
                let questions: Vec<Question> = params
                    .get("questions")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|q| {
                                Some(Question {
                                    id: q.get("id")?.as_str()?.to_string(),
                                    question: q
                                        .get("question")
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .to_string(),
                                    header: q
                                        .get("header")
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .to_string(),
                                    detail: q
                                        .get("detail")
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .to_string(),
                                    plan_approve: q
                                        .get("intent")
                                        .and_then(|i| i.get("approve"))
                                        .and_then(Value::as_str)
                                        .map(String::from),
                                    options: q
                                        .get("options")
                                        .and_then(Value::as_array)
                                        .map(|o| {
                                            o.iter()
                                                .filter_map(|v| {
                                                    v.get("label")
                                                        .and_then(Value::as_str)
                                                        .map(String::from)
                                                })
                                                .collect()
                                        })
                                        .unwrap_or_default(),
                                    multi_select: q
                                        .get("multiSelect")
                                        .and_then(Value::as_bool)
                                        .unwrap_or(false),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                // 空 questions 的畸形请求会让绘制和按键路径都索引越界；
                // 与 busy 一样 fail-closed，直接回错误而不是打开空卡。
                if questions.is_empty() {
                    self.respond(id, json!({ "error": "no questions" }));
                    return;
                }
                if self.has_dialog() {
                    self.respond(id, json!({ "error": "busy" }));
                    return;
                }
                let n = questions.len();
                self.dialog = Dialog::Ask(AskDialog {
                    request_id: id,
                    questions,
                    current: 0,
                    answers: vec![Vec::new(); n],
                    cursors: vec![0; n],
                    feedback: String::new(),
                    taking_feedback: false,
                    detail_scroll: 0,
                    custom_text: String::new(),
                    taking_text: false,
                    parked: false,
                });
            }
            _ => {
                // Unknown server request: answer null (bridge treats as delegate).
                self.respond(id, Value::Null);
            }
        }
    }

    /// Returns true when this event actually changed something. A mouse event
    /// that changed nothing must not trigger a redraw — with motion reporting on
    /// that alone made the screen judder under a moving pointer.
    fn handle_mouse(&mut self, mouse: crossterm::event::MouseEvent) -> bool {
        use crossterm::event::MouseEventKind;
        if self.has_dialog() {
            match mouse.kind {
                // 滚轮 = 方向键：所有弹窗复用各自的 ↑/↓ 处理（移动选择或滚动）。
                MouseEventKind::ScrollUp => {
                    self.dialog_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
                    return true;
                }
                MouseEventKind::ScrollDown => {
                    self.dialog_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
                    return true;
                }
                MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                    let hit = self
                        .dialog_hits
                        .iter()
                        .find(|hit| hit.contains(mouse.column, mouse.row))
                        .copied();
                    return match hit {
                        Some(DialogHit::Select { index, .. }) => self.dialog_click_select(index),
                        Some(DialogHit::Key { ch, .. }) => {
                            self.dialog_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
                            true
                        }
                        // 未命中选项行时忽略（不抢焦点、不关弹窗），与旧行为一致。
                        None => false,
                    };
                }
                _ => return false,
            }
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.focus = Focus::Scrollback;
                self.follow_selection = false;
                self.scroll = self.scroll.saturating_add(3);
                true
            }
            MouseEventKind::ScrollDown => {
                self.focus = Focus::Scrollback;
                self.follow_selection = false;
                self.scroll = self.scroll.saturating_sub(3);
                true
            }
            MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                // composer_bottom 为 0 说明还没画过第一帧，退回旧的"顶部以下
                // 皆 composer"启发式，行为与引入几何字段前一致。
                let in_box = mouse.row >= self.composer_top
                    && (self.composer_bottom == 0 || mouse.row < self.composer_bottom);
                if !in_box {
                    // Only move focus. This used to also slam `scroll` to 0, so a
                    // click near the composer yanked the viewport to the bottom —
                    // which read as the UI jumping while trying to select text.
                    let changed = self.focus != Focus::Scrollback;
                    self.focus = Focus::Scrollback;
                    self.follow_selection = true;
                    return changed;
                }
                let mut changed = self.focus != Focus::Prompt;
                self.focus = Focus::Prompt;
                // 点击文本区内（非边框）时把光标搬到点击处：grok 的
                // click-to-position，免去长按方向键。
                if mouse.row > self.composer_top
                    && (self.composer_bottom == 0 || mouse.row + 1 < self.composer_bottom)
                {
                    let layout = crate::ui::composer_layout(
                        &self.input,
                        self.cursor,
                        self.composer_inner_width,
                    );
                    let visible_start =
                        crate::ui::composer_visible_start(layout.rows.len(), layout.cursor_row);
                    let row = visible_start + (mouse.row - self.composer_top - 1) as usize;
                    // 左边框 1 列 + "› " 前缀 2 列。
                    let col = (mouse.column as usize).saturating_sub(3);
                    let offset = crate::ui::composer_offset_at(
                        &self.input,
                        self.composer_inner_width,
                        row,
                        col,
                    );
                    if offset != self.cursor {
                        self.cursor = offset;
                        self.typing_run = false;
                        self.leave_history_navigation();
                        changed = true;
                    }
                }
                changed
            }
            _ => false,
        }
    }

    /// 鼠标点击弹窗选项行：把选择光标搬到被点项，再注入一次 Enter，
    /// 与"方向键移动 + 回车"的键盘路径完全同码。个别弹窗语义不同：
    /// Ask 多选只切换不跳题，ProviderList 只移动选择（Enter 本无操作），
    /// Theme 点击即预览（方向键本来就实时换肤，点击不能跳过预览直接保存）。
    fn dialog_click_select(&mut self, index: usize) -> bool {
        let mut confirm = true;
        match &mut self.dialog {
            Dialog::Approval(d) => {
                d.selected = index.min(d.options.len().saturating_sub(1));
            }
            Dialog::Ask(d) => {
                let cur = d.current.min(d.questions.len().saturating_sub(1));
                let opts = d.questions[cur].options.len();
                if index >= opts {
                    return false;
                }
                d.cursors[cur] = index;
                if d.questions[cur].multi_select {
                    if d.answers[cur].contains(&index) {
                        d.answers[cur].retain(|i| *i != index);
                    } else {
                        d.answers[cur].push(index);
                    }
                    confirm = false;
                } else {
                    d.answers[cur] = vec![index];
                }
            }
            Dialog::Theme(t) => {
                t.selected = index.min(1);
                self.theme = if t.selected == 0 {
                    crate::theme::theme_for("dark")
                } else {
                    crate::theme::theme_for("light")
                };
            }
            Dialog::Palette(p) => {
                p.selected = index.min(p.visible.len().saturating_sub(1));
            }
            Dialog::Model(m) => {
                // Model 的 selected 是 rows 下标（渲染期已换算）。
                if index < m.rows.len() {
                    m.selected = index;
                }
            }
            Dialog::Effort(e) => {
                e.selected = index.min(e.rows.len().saturating_sub(1));
            }
            Dialog::Resume(p) => {
                p.selected = index.min(p.items.len().saturating_sub(1));
            }
            Dialog::Rewind(r) => {
                r.selected = index.min(r.items.len().saturating_sub(1));
            }
            Dialog::FilePicker(f) => {
                f.selected = index.min(f.visible.len().saturating_sub(1));
            }
            Dialog::History(v) => {
                v.selected = index.min(v.visible.len().saturating_sub(1));
            }
            Dialog::ProviderList(v) => {
                v.selected = index.min(v.items.len().saturating_sub(1));
                confirm = false;
            }
            Dialog::Provider(w) => match w.step {
                ProviderStep::Type => w.type_sel = index.min(1),
                ProviderStep::Known => {
                    w.catalog_sel = index.min(w.catalog.len().saturating_sub(1));
                }
                ProviderStep::Api if !w.protocols.is_empty() => {
                    w.proto_sel = index.min(w.protocols.len().saturating_sub(1));
                }
                ProviderStep::Models => {
                    w.model_cursor = index.min(w.models.len().saturating_sub(1));
                    w.sync_models_focus();
                }
                _ => confirm = false,
            },
            _ => confirm = false,
        }
        if confirm {
            self.dialog_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        }
        true
    }

    fn handle_key(&mut self, ev: Event) {
        // Bracketed paste: terminals wrap IME commits and Cmd/Ctrl+V paste in
        // ESC[200~ ... ESC[201~. crossterm decodes that to Event::Paste, and
        // dropping it here is what made Chinese input (and any pasted text)
        // never reach the composer on modern terminals.
        if let Event::Paste(text) = ev {
            self.handle_paste(text);
            return;
        }
        let Event::Key(key) = ev else { return };
        if key.kind != KeyEventKind::Press {
            return;
        }

        // ---- global quit (works even while a dialog is open) ----
        // Double-press within 1s, like Grok: a single stray Ctrl+Q used to take
        // the draft down with it. Ctrl+D only quits from the composer — in the
        // scrollback it is half-page-down (see the navigation block below).
        let quit_chord = key.modifiers.contains(KeyModifiers::CONTROL)
            && (key.code == KeyCode::Char('q')
                || key.code == KeyCode::Char('d') && self.focus != Focus::Scrollback);
        if quit_chord {
            if self.armed(Confirm::Quit) {
                self.quit = true;
            } else {
                self.arm(Confirm::Quit);
                self.set_notice("press again to quit");
            }
            return;
        }

        // ---- blocking dialogs take over the keyboard (docs/01 section 2.4) ----
        // Parked approval/question cards stay visible while normal scrollback
        // navigation owns the keyboard. Tab returns focus to the card.
        let parked = matches!(&self.dialog, Dialog::Approval(d) if d.parked)
            || matches!(&self.dialog, Dialog::Ask(d) if d.parked);
        if parked {
            if key.code == KeyCode::Tab {
                match &mut self.dialog {
                    Dialog::Approval(d) => d.parked = false,
                    Dialog::Ask(d) => d.parked = false,
                    _ => {}
                }
                return;
            }
            // fall through to normal scrollback handling below
        } else if self.has_dialog() {
            self.dialog_key(key);
            return;
        }

        // ---- demo-only synthetic dialogs for testing ----
        if self.demo {
            if key.code == KeyCode::F(2) {
                self.open_dialog(
                    "demo-approve".into(),
                    "ui/approve",
                    &json!({ "toolName": "bash", "reason": "shell command", "input": {"command": "cargo test"}, "options": ["allowed-once", "rejected"] }),
                );
                return;
            }
            if key.code == KeyCode::F(3) {
                self.open_dialog(
                    "demo-ask".into(),
                    "ui/ask-user",
                    &json!({ "questions": [
                        { "id": "q1", "question": "选一个颜色？", "header": "主题", "options": [{"label": "蓝色"}, {"label": "绿色"}, {"label": "紫色"}] },
                        { "id": "q2", "question": "多选：喜欢哪些功能？", "options": [{"label": "审批"}, {"label": "子代理"}, {"label": "计划"}], "multiSelect": true }
                    ] }),
                );
                return;
            }
        }

        // ---- Esc semantics (docs/01 section 2.5) ----
        // Rapid ESC ESC collapses to Alt+Esc on most terminals; treat it as
        // the second press so double-Esc works whether spaced or instant.
        if key.code == KeyCode::Esc {
            let alt = key.modifiers.contains(KeyModifiers::ALT);
            if self.is_running() {
                self.cancel_now();
                return;
            }
            if !self.input.is_empty() {
                if alt {
                    let dropped = self.take_input();
                    self.history.push(dropped);
                    self.leave_history_navigation();
                    self.esc = EscArm::None;
                } else {
                    let now = Instant::now();
                    match self.esc {
                        EscArm::ClearArmed(t)
                            if now.duration_since(t).as_millis() <= DOUBLE_ESC_MS =>
                        {
                            let dropped = self.take_input();
                            self.history.push(dropped);
                            self.leave_history_navigation();
                            self.esc = EscArm::None;
                        }
                        _ => self.esc = EscArm::ClearArmed(now),
                    }
                }
                return;
            }
            if !self.transcript.is_empty() {
                let in_grace = self
                    .cancel_grace
                    .map(|t| t.elapsed().as_millis() < 1000)
                    .unwrap_or(false);
                if in_grace {
                    self.esc = EscArm::None;
                    return;
                }
                if alt {
                    self.esc = EscArm::None;
                    self.open_rewind();
                } else {
                    let now = Instant::now();
                    match self.esc {
                        EscArm::RewindArmed(t)
                            if now.duration_since(t).as_millis() <= DOUBLE_ESC_MS =>
                        {
                            self.esc = EscArm::None;
                            self.open_rewind();
                        }
                        _ => self.esc = EscArm::RewindArmed(now),
                    }
                }
                return;
            }
            self.esc = EscArm::None;
            return;
        }

        // ---- Ctrl+C: clear draft → cancel turn → quit (docs/01 section 2.5) ----
        // 对齐常见 harness 手感：空闲且输入框为空时 Ctrl+C 是退出（双按
        // 确认，与 Ctrl+Q 同一条确认臂）；有草稿先清草稿，运行中先清再取消。
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if self.is_running() && !self.input.is_empty() {
                self.clear_input();
                self.leave_history_navigation();
            } else if self.is_running() {
                self.cancel_now();
            } else if !self.input.is_empty() {
                self.clear_input();
                self.leave_history_navigation();
            } else if self.armed(Confirm::Quit) {
                self.quit = true;
            } else {
                self.arm(Confirm::Quit);
                self.set_notice("press again to quit");
            }
            return;
        }

        // ---- keyboard shortcuts (Grok Ctrl+X / Ctrl+.) ----
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('x') | KeyCode::Char('.'))
        {
            self.open_shortcuts();
            return;
        }

        // ---- session shortcuts ----
        // /new discards the whole conversation, so Grok gates it the same way
        // as quit: press twice inside a second.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('n') {
            if self.armed(Confirm::NewSession) {
                self.run_command("/new");
            } else {
                self.arm(Confirm::NewSession);
                self.set_notice("press again for a new session");
            }
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            self.run_command("/resume");
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('r') {
            self.open_history();
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char(';') | KeyCode::Char('\''))
        {
            self.open_queue();
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && key.code == KeyCode::Char('4')
            && self.term_kind.is_vscode_family()
        {
            self.open_queue();
            return;
        }

        let send_now = if self.term_kind.is_vscode_family() {
            key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('l')
        } else {
            key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Enter | KeyCode::Char('i'))
                || self.term_kind == TermKind::AppleTerminal
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                    && key.code == KeyCode::Char('o')
        };
        if send_now {
            if !self.is_running() {
                return;
            }
            if self.input.is_empty() {
                let item_id = self.queued_items().next().map(|item| item.id.clone());
                if let Some(item_id) = item_id {
                    self.send_now_item(item_id);
                }
            } else {
                let text = self.take_input();
                self.send_now_text(text);
            }
            return;
        }

        // ---- always-approve toggle (Grok Ctrl+O) ----
        // Deliberately after `send_now`: on Apple Terminal Ctrl+O *is* the
        // send-now chord and has already been consumed above.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('o') {
            self.run_command("/always-approve");
            return;
        }

        // ---- Ctrl+M is contextual: multiline in the composer, model picker elsewhere. ----
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('m') {
            if self.focus == Focus::Prompt {
                self.toggle_multiline();
            } else {
                self.catalog_for_picker = true;
                self.status = "loading models".into();
                let _ = self.cmd_tx.send(Cmd::FetchCatalog {
                    session_id: self.session_id.clone(),
                });
            }
            return;
        }

        // ---- todos pane (Grok Ctrl+T) ----
        // Theme switching used to sit here, which stole Grok's todos binding.
        // It lives on /theme now (that picker has live preview + Esc revert).
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('t') {
            self.open_todos();
            return;
        }

        // ---- tasks pane (docs/01 section 10) ----
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('g') {
            let _ = self.cmd_tx.send(Cmd::FetchJobs);
            return;
        }

        // ---- command palette (docs/01 section 2.6) ----
        // '?' is the palette alt binding only outside the composer, so a
        // question mark in a prompt keeps typing normally.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('p')
            || key.code == KeyCode::Char('?') && self.focus == Focus::Scrollback
        {
            self.open_palette();
            return;
        }

        // ---- thinking 全局折叠/展开（Grok Ctrl+E）----
        // Contextual like Grok's Ctrl+M: in the composer Ctrl+E is the readline
        // end-of-line motion, so only the scrollback gets the fold toggle.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && key.code == KeyCode::Char('e')
            && self.focus == Focus::Scrollback
        {
            let any_open = self
                .transcript
                .cells
                .iter()
                .any(|c| c.kind == CellKind::Thinking && !c.folded);
            let target = !any_open;
            for c in &mut self.transcript.cells {
                if c.kind == CellKind::Thinking {
                    c.folded = !target;
                }
            }
            self.set_notice(if target {
                "thinking expanded"
            } else {
                "thinking collapsed"
            });
            return;
        }

        // ---- Explicit line break works in either composer mode. ----
        if key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::SHIFT) {
            self.leave_history_navigation();
            self.insert_char('\n');
            return;
        }

        // ---- slash commands (dispatch without a model turn) ----
        if key.code == KeyCode::Enter && !self.input.is_empty() && self.input.starts_with('/') {
            let cmd = self.take_input();
            self.run_command(&cmd);
            return;
        }

        // ---- send / queue / send-now ----
        if key.code == KeyCode::Enter {
            let multiline_send = self.multiline && key.modifiers.contains(KeyModifiers::ALT);
            if self.multiline && !multiline_send {
                if self.input.is_empty() && self.is_running() {
                    let item_id = self.queued_items().next().map(|item| item.id.clone());
                    if let Some(item_id) = item_id {
                        self.send_now_item(item_id);
                        return;
                    }
                }
                self.leave_history_navigation();
                self.insert_char('\n');
                return;
            }
            if self.input.is_empty() {
                if self.is_running() {
                    let item_id = self.queued_items().next().map(|item| item.id.clone());
                    if let Some(item_id) = item_id {
                        self.send_now_item(item_id);
                    }
                }
                return;
            }
            let text = self.take_input();
            self.send_input(text);
            return;
        }

        // ---- mode cycle: Normal → Plan → Always-approve ----
        if key.code == KeyCode::BackTab {
            self.cycle_permission_mode();
            return;
        }

        // ---- focus ----
        if key.code == KeyCode::Tab {
            self.focus = match self.focus {
                Focus::Prompt => {
                    self.follow_selection = true;
                    Focus::Scrollback
                }
                Focus::Scrollback => Focus::Prompt,
            };
            return;
        }

        // ---- viewport scrolling works from either focus ----
        // These must not be gated on scrollback focus: the mouse wheel used to be
        // the only way to scroll while typing, and mouse reporting is off by
        // default now. After /resume the focus is the prompt, so without these
        // there is no way to reach the replayed history at all.
        //
        // Shift+arrows exist because laptops have no PageUp/PageDown key — Fn+↑
        // is not a binding anyone discovers. Shift+Up/Down are unused elsewhere
        // (Shift+Left/Right are turn navigation, handled in the scrollback block).
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let scroll_by = match key.code {
            KeyCode::PageUp => Some(10isize),
            KeyCode::PageDown => Some(-10),
            KeyCode::Up if shift => Some(3),
            KeyCode::Down if shift => Some(-3),
            _ => None,
        };
        if let Some(step) = scroll_by {
            self.follow_selection = false;
            self.scroll = if step > 0 {
                self.scroll.saturating_add(step as usize)
            } else {
                self.scroll.saturating_sub(step.unsigned_abs())
            };
            return;
        }

        // ---- scrollback navigation ----
        if self.focus == Focus::Scrollback {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            // Viewport-only scrolling: these move the window without touching
            // the selection (Grok Ctrl+J/K by line, Ctrl+U/D by half page).
            if ctrl {
                let step = match key.code {
                    KeyCode::Char('k') => Some(1isize),
                    KeyCode::Char('j') => Some(-1),
                    KeyCode::Char('u') => Some(10),
                    KeyCode::Char('d') => Some(-10),
                    _ => None,
                };
                if let Some(step) = step {
                    self.follow_selection = false;
                    self.scroll = if step > 0 {
                        self.scroll.saturating_add(step as usize)
                    } else {
                        self.scroll.saturating_sub(step.unsigned_abs())
                    };
                    return;
                }
            }
            match key.code {
                // Shift+H/L (and Shift+arrows) walk turns; Shift+J/K walk replies.
                KeyCode::Char('H') | KeyCode::Left
                    if key.modifiers.contains(KeyModifiers::SHIFT) =>
                {
                    self.follow_selection = true;
                    self.transcript
                        .move_selection_to_kind(CellKind::User, false);
                }
                KeyCode::Char('L') | KeyCode::Right
                    if key.modifiers.contains(KeyModifiers::SHIFT) =>
                {
                    self.follow_selection = true;
                    self.transcript.move_selection_to_kind(CellKind::User, true);
                }
                KeyCode::Char('K') if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    self.follow_selection = true;
                    self.transcript
                        .move_selection_to_kind(CellKind::Assistant, false);
                }
                KeyCode::Char('J') if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    self.follow_selection = true;
                    self.transcript
                        .move_selection_to_kind(CellKind::Assistant, true);
                }
                KeyCode::Char('E') if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    let expanded = self.transcript.toggle_all_folds();
                    self.set_notice(if expanded {
                        "all expanded"
                    } else {
                        "all collapsed"
                    });
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.follow_selection = true;
                    self.transcript.move_selection(-1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.follow_selection = true;
                    self.transcript.move_selection(1);
                }
                KeyCode::Left | KeyCode::Char('h') => self.set_selected_fold(true),
                KeyCode::Right | KeyCode::Char('l') => self.set_selected_fold(false),
                KeyCode::Char('e') | KeyCode::Char(' ') => {
                    if let Some(i) = self.transcript.selected {
                        self.transcript.toggle_fold(i);
                    }
                }
                KeyCode::Char('r') => {
                    if let Some(i) = self.transcript.selected {
                        self.transcript.toggle_raw(i);
                    }
                }
                KeyCode::Char('g') | KeyCode::Home => {
                    self.follow_selection = true;
                    self.select_first_block();
                }
                KeyCode::Char('G') | KeyCode::End => {
                    self.follow_selection = true;
                    self.select_last_block();
                }
                // Enter opens the block viewer; Ctrl+F is Grok's alt binding for
                // the same thing. A bare `f` still auto-focuses the composer.
                KeyCode::Enter | KeyCode::Char('f') if key.code == KeyCode::Enter || ctrl => {
                    if let Some(i) = self.transcript.selected {
                        let cell = self.transcript.cells[i].clone();
                        if let Some(link) = cell.link.clone() {
                            self.dialog = Dialog::Subagent(SubagentView {
                                child_id: link,
                                scroll: 0,
                            });
                        } else if matches!(cell.kind, CellKind::Tool | CellKind::ToolResult) {
                            self.dialog = Dialog::Block(BlockView { cell, scroll: 0 });
                        }
                    }
                }
                KeyCode::Char('y') => {
                    if let Some(i) = self.transcript.selected {
                        let text = self.transcript.cells[i].text.clone();
                        self.copy_text(text);
                    }
                }
                KeyCode::Char('Y') => {
                    if let Some(i) = self.transcript.selected {
                        let title = self.transcript.cells[i].title.clone();
                        self.copy_text(title);
                    }
                }
                // Simple-mode auto-focus: a bare letter jumps to the composer
                // and starts typing. Chords must not, or Ctrl+V in the
                // scrollback would insert a literal "v".
                KeyCode::Char(c)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    self.focus = Focus::Prompt;
                    self.leave_history_navigation();
                    self.insert_char(c);
                }
                _ => {}
            }
            return;
        }

        // ---- prompt editing ----
        // readline motions and kills first, so a Ctrl/Alt chord never falls
        // through to the plain-character arm below.
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            // kills
            KeyCode::Char('w') if ctrl => {
                self.leave_history_navigation();
                self.delete_word_before();
                return;
            }
            KeyCode::Backspace if alt || ctrl => {
                self.leave_history_navigation();
                self.delete_word_before();
                return;
            }
            KeyCode::Char('d') if alt => {
                self.leave_history_navigation();
                self.delete_word_after();
                return;
            }
            KeyCode::Delete if alt || ctrl => {
                self.leave_history_navigation();
                self.delete_word_after();
                return;
            }
            KeyCode::Char('u') if ctrl => {
                self.leave_history_navigation();
                self.delete_to_line_start();
                return;
            }
            KeyCode::Char('k') if ctrl => {
                self.leave_history_navigation();
                self.delete_to_line_end();
                return;
            }
            KeyCode::Char('z') if ctrl => {
                self.undo_input();
                return;
            }
            // motions
            KeyCode::Char('a') if ctrl => {
                self.cursor_line_start();
                return;
            }
            KeyCode::Char('e') if ctrl => {
                self.cursor_line_end();
                return;
            }
            KeyCode::Home => {
                self.cursor_line_start();
                return;
            }
            KeyCode::End => {
                self.cursor_line_end();
                return;
            }
            KeyCode::Left if alt || ctrl => {
                self.cursor_word_left();
                return;
            }
            KeyCode::Right if alt || ctrl => {
                self.cursor_word_right();
                return;
            }
            KeyCode::Char('b') if alt => {
                self.cursor_word_left();
                return;
            }
            KeyCode::Char('f') if alt => {
                self.cursor_word_right();
                return;
            }
            KeyCode::Left => {
                self.cursor_left();
                return;
            }
            KeyCode::Right => {
                self.cursor_right();
                return;
            }
            KeyCode::Delete => {
                self.leave_history_navigation();
                self.delete_forward();
                return;
            }
            _ => {}
        }
        match key.code {
            KeyCode::Char('/') if self.input.is_empty() => {
                self.leave_history_navigation();
                self.open_palette();
            }
            KeyCode::Char('@') => {
                self.leave_history_navigation();
                let files = list_files(&self.workspace);
                let visible = fuzzy_filter(&files, "");
                self.clamp_cursor();
                self.at_fragment_start = Some(self.cursor);
                self.insert_char('@');
                self.dialog = Dialog::FilePicker(FilePicker {
                    files,
                    query: String::new(),
                    selected: 0,
                    visible,
                });
            }
            KeyCode::Char(c) => {
                self.leave_history_navigation();
                self.insert_char(c);
            }
            KeyCode::Backspace => {
                self.leave_history_navigation();
                self.backspace();
            }
            // Up/Down walk the lines of a multi-line draft; on a single line
            // (or at the first/last line) they fall through to prompt history.
            KeyCode::Up => {
                if !self.move_cursor_line(-1) {
                    self.history_previous();
                }
            }
            KeyCode::Down => {
                if !self.move_cursor_line(1) {
                    self.history_next();
                }
            }
            KeyCode::Enter => {}
            _ => {}
        }
    }

    /// plan-review intent (docs/01 section 3.4): the plan markdown renders
    /// in a scrolled window; a approves, s requests changes with typed
    /// feedback, c comments on the line under the cursor, y copies the plan,
    /// q abandons, arrows scroll the plan.
    fn plan_review_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::KeyCode;
        let (request_id, qid, approve, other, taking, fb, scroll) = {
            let Dialog::Ask(d) = &self.dialog else { return };
            let cur = d.current.min(d.questions.len().saturating_sub(1));
            let q = &d.questions[cur];
            (
                d.request_id.clone(),
                q.id.clone(),
                q.plan_approve.clone().unwrap_or_default(),
                q.options
                    .iter()
                    .find(|o| Some(o.as_str()) != q.plan_approve.as_deref())
                    .cloned()
                    .unwrap_or_default(),
                d.taking_feedback,
                d.feedback.clone(),
                d.detail_scroll,
            )
        };
        if taking {
            match key.code {
                KeyCode::Enter => {
                    self.dialog = Dialog::None;
                    self.respond(
                        request_id,
                        json!({ "answers": [ { "id": qid, "selected": [other], "custom": fb } ] }),
                    );
                }
                KeyCode::Esc => {
                    if let Dialog::Ask(d) = &mut self.dialog {
                        d.taking_feedback = false;
                    }
                }
                KeyCode::Backspace => {
                    if let Dialog::Ask(d) = &mut self.dialog {
                        d.feedback.pop();
                    }
                }
                KeyCode::Char(c) => {
                    if let Dialog::Ask(d) = &mut self.dialog {
                        d.feedback.push(c);
                    }
                }
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Char('a') => {
                self.dialog = Dialog::None;
                self.respond(
                    request_id,
                    json!({ "answers": [ { "id": qid, "selected": [approve] } ] }),
                );
            }
            KeyCode::Char('q') => {
                self.dialog = Dialog::None;
                self.respond(request_id, json!({ "answers": [] }));
            }
            KeyCode::Char('y') => {
                let detail = {
                    let Dialog::Ask(d) = &self.dialog else { return };
                    let cur = d.current.min(d.questions.len().saturating_sub(1));
                    d.questions[cur].detail.clone()
                };
                self.copy_text(detail);
            }
            KeyCode::Char('c') => {
                // Line comment: enter feedback mode prefixed with the line
                // under the detail cursor; comments ride the request-changes
                // (s) answer's custom field.
                let line_no = {
                    let Dialog::Ask(d) = &self.dialog else { return };
                    d.detail_scroll + 1
                };
                if let Dialog::Ask(d) = &mut self.dialog {
                    d.feedback = format!("L{}: ", line_no);
                    d.taking_feedback = true;
                }
            }
            KeyCode::Char('s') => {
                if let Dialog::Ask(d) = &mut self.dialog {
                    d.taking_feedback = true;
                }
            }
            KeyCode::Enter => {}
            KeyCode::Esc => {
                if let Dialog::Ask(d) = &mut self.dialog {
                    d.parked = true;
                }
                self.focus = Focus::Scrollback;
            }
            KeyCode::PageUp | KeyCode::Up | KeyCode::Char('k') => {
                if let Dialog::Ask(d) = &mut self.dialog {
                    d.detail_scroll =
                        scroll.saturating_sub(if key.code == KeyCode::PageUp { 6 } else { 1 });
                }
            }
            KeyCode::PageDown | KeyCode::Down | KeyCode::Char('j') => {
                if let Dialog::Ask(d) = &mut self.dialog {
                    d.detail_scroll =
                        scroll.saturating_add(if key.code == KeyCode::PageDown { 6 } else { 1 });
                }
            }
            _ => {}
        }
    }

    /// Route bracketed-paste text to the control that currently owns the
    /// keyboard. Outside dialogs this is the prompt composer; inside dialogs
    /// it is the active text field (ask_user free text / plan feedback /
    /// filter boxes). A parked approval card hands the keyboard back to the
    /// scrollback, so paste goes to the composer exactly like typed chars.
    fn handle_paste(&mut self, text: String) {
        // Decide the target before taking a mutable borrow on the dialog, so
        // the composer path can go through `insert_str` and land at the caret.
        let to_composer = match &self.dialog {
            Dialog::None => true,
            Dialog::Approval(d) => d.parked,
            Dialog::Ask(d) => d.parked,
            _ => false,
        };
        if to_composer {
            self.focus = Focus::Prompt;
            self.leave_history_navigation();
            self.insert_str(&text);
            return;
        }
        match &mut self.dialog {
            Dialog::History(d) => {
                d.query.push_str(&text);
                let query = d.query.clone();
                d.visible = history_matches(&self.history, &query);
                d.selected = 0;
            }
            Dialog::Ask(d) => {
                if d.taking_text {
                    d.custom_text.push_str(&text);
                } else if d.taking_feedback {
                    d.feedback.push_str(&text);
                }
            }
            Dialog::Model(d) => d.filter.push_str(&text),
            Dialog::Queue(d) if d.editing => d.draft.push_str(&text),
            Dialog::FilePicker(d) => d.query.push_str(&text),
            Dialog::Palette(d) => d.filter.push_str(&text),
            Dialog::Shortcuts(d) => d.filter.push_str(&text),
            _ => {}
        }
    }

    fn dialog_key(&mut self, key: crossterm::event::KeyEvent) {
        let has_ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match &mut self.dialog {
            Dialog::Approval(d) => {
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        d.selected = d.selected.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        d.selected = (d.selected + 1).min(d.options.len().saturating_sub(1));
                    }
                    KeyCode::Char(c) if c.is_ascii_digit() => {
                        let n = c.to_digit(10).unwrap_or(10) as usize;
                        if n >= 1 && n <= d.options.len() {
                            d.selected = n - 1;
                        }
                    }
                    KeyCode::Enter => {
                        let chosen = d
                            .options
                            .get(d.selected)
                            .cloned()
                            .unwrap_or_else(|| "cancelled".into());
                        let id = d.request_id.clone();
                        self.dialog = Dialog::None;
                        if chosen == "always-allow" {
                            // 本次放行，同时把完整模式切换为 Always-approve。
                            self.permission_mode = PermissionMode::AlwaysApprove;
                            self.apply_permission_mode();
                            self.respond(
                                id,
                                json!({ "outcome": "allowed-once", "remember": "always" }),
                            );
                        } else {
                            self.respond(id, json!({ "outcome": chosen }));
                        }
                    }
                    // docs/01 section 2.4: Esc parks focus in the scrollback
                    // without answering; Tab returns to the card.
                    KeyCode::Esc => {
                        d.parked = true;
                        self.focus = Focus::Scrollback;
                    }
                    KeyCode::Char('c') if has_ctrl => {
                        let id = d.request_id.clone();
                        self.dialog = Dialog::None;
                        self.respond(id, json!({ "outcome": "cancelled" }));
                    }
                    _ => {}
                }
            }
            Dialog::Ask(_) => {
                let is_plan = match &self.dialog {
                    Dialog::Ask(d) => {
                        let cur = d.current.min(d.questions.len().saturating_sub(1));
                        d.questions[cur].plan_approve.is_some()
                    }
                    _ => false,
                };
                if is_plan {
                    self.plan_review_key(key);
                    return;
                }
                let d = match &mut self.dialog {
                    Dialog::Ask(d) => d,
                    _ => return,
                };
                let n = d.questions.len();
                let cur = d.current.min(n.saturating_sub(1));
                let opts = d.questions[cur].options.len();
                let multi = d.questions[cur].multi_select;
                match key.code {
                    KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('[') => {
                        d.current = cur.saturating_sub(1);
                    }
                    KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(']') => {
                        d.current = (cur + 1).min(n.saturating_sub(1));
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        d.cursors[cur] = d.cursors[cur].saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if opts > 0 {
                            d.cursors[cur] = (d.cursors[cur] + 1).min(opts - 1);
                        }
                    }
                    KeyCode::Char(c) if c.is_ascii_digit() => {
                        let idx = c.to_digit(10).unwrap_or(10) as usize;
                        if idx >= 1 && idx <= opts {
                            d.cursors[cur] = idx - 1;
                            if multi {
                                let cursor = d.cursors[cur];
                                if d.answers[cur].contains(&cursor) {
                                    d.answers[cur].retain(|i| *i != cursor);
                                } else {
                                    d.answers[cur].push(cursor);
                                }
                            } else {
                                d.answers[cur] = vec![d.cursors[cur]];
                            }
                        }
                    }
                    KeyCode::Char(' ') if multi => {
                        let idx = d.cursors[cur].min(opts.saturating_sub(1));
                        if d.answers[cur].contains(&idx) {
                            d.answers[cur].retain(|i| *i != idx);
                        } else {
                            d.answers[cur].push(idx);
                        }
                    }
                    // Free-text answer (docs/01 section 2.4, z key): custom
                    // overrides the selection for single-select questions.
                    KeyCode::Char('z') => {
                        d.taking_text = true;
                    }
                    KeyCode::Char(c) if d.taking_text => {
                        d.custom_text.push(c);
                    }
                    KeyCode::Backspace => {
                        if d.taking_text {
                            d.custom_text.pop();
                        }
                    }
                    KeyCode::Enter if d.taking_text && d.custom_text.is_empty() => {}
                    KeyCode::Enter => {
                        if d.taking_text {
                            // submit with the typed answer
                            let custom = std::mem::take(&mut d.custom_text);
                            let qid = d.questions[cur].id.clone();
                            let id = d.request_id.clone();
                            self.dialog = Dialog::None;
                            self.respond(
                                id,
                                json!({ "answers": [ { "id": qid, "selected": [], "custom": custom } ] }),
                            );
                        } else if opts > 0 && d.answers[cur].is_empty() {
                            d.answers[cur] = vec![d.cursors[cur].min(opts - 1)];
                            if cur + 1 < n {
                                d.current = cur + 1;
                            } else {
                                let questions = d.questions.clone();
                                let answers: Vec<Value> = questions
                                    .iter()
                                    .enumerate()
                                    .map(|(i, q)| {
                                        let selected: Vec<String> = d.answers[i]
                                            .iter()
                                            .filter_map(|idx| q.options.get(*idx).cloned())
                                            .collect();
                                        json!({ "id": q.id, "selected": selected })
                                    })
                                    .collect();
                                let id = d.request_id.clone();
                                self.dialog = Dialog::None;
                                self.respond(id, json!({ "answers": answers }));
                            }
                        } else if cur + 1 < n {
                            d.current = cur + 1;
                        } else {
                            // Submit: build the canonical answer shape.
                            let questions = d.questions.clone();
                            let answers: Vec<Value> = questions
                                .iter()
                                .enumerate()
                                .map(|(i, q)| {
                                    let selected: Vec<String> = d.answers[i]
                                        .iter()
                                        .filter_map(|idx| q.options.get(*idx).cloned())
                                        .collect();
                                    json!({ "id": q.id, "selected": selected })
                                })
                                .collect();
                            let id = d.request_id.clone();
                            self.dialog = Dialog::None;
                            self.respond(id, json!({ "answers": answers }));
                        }
                    }
                    KeyCode::Esc => {
                        if d.taking_text {
                            d.taking_text = false;
                            d.custom_text.clear();
                        } else if d.has_pending_input() {
                            d.custom_text.clear();
                            d.feedback.clear();
                        } else {
                            d.parked = true;
                            self.focus = Focus::Scrollback;
                        }
                    }
                    _ => {}
                }
                let _ = has_ctrl;
            }
            Dialog::FilePicker(f) => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    f.selected = f.selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    f.selected = (f.selected + 1).min(f.visible.len().saturating_sub(1));
                }
                KeyCode::Char(c) => {
                    f.query.push(c);
                    f.visible = fuzzy_filter(&f.files, &f.query);
                    f.selected = 0;
                }
                KeyCode::Backspace => {
                    f.query.pop();
                    f.visible = fuzzy_filter(&f.files, &f.query);
                    f.selected = 0;
                }
                KeyCode::Enter | KeyCode::Tab => {
                    if let Some(idx) = f.visible.get(f.selected) {
                        let path = f.files[*idx].clone();
                        if let Some(start) = self.at_fragment_start {
                            // Replace just the "@" we inserted, not everything
                            // after it — the caret can sit mid-draft now.
                            let start = start.min(self.input.len());
                            let end = self.next_boundary(start).min(self.input.len());
                            let insert = format!("{path} ");
                            self.input.replace_range(start..end, &insert);
                            self.cursor = start + insert.len();
                            self.typing_run = false;
                        }
                        self.at_fragment_start = None;
                        self.dialog = Dialog::None;
                        self.focus = Focus::Prompt;
                    }
                }
                KeyCode::Esc => {
                    self.at_fragment_start = None;
                    self.dialog = Dialog::None;
                }
                _ => {}
            },
            Dialog::Tasks(t) => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    t.selected = t.selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    t.selected = (t.selected + 1).min(t.rows.len().saturating_sub(1));
                }
                KeyCode::Char('r') => {
                    let _ = self.cmd_tx.send(Cmd::FetchJobs);
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.dialog = Dialog::None;
                }
                _ => {}
            },
            Dialog::Todos(view) => {
                let last = self.transcript.todos.len().saturating_sub(1);
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        view.selected = view.selected.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        view.selected = (view.selected + 1).min(last);
                    }
                    KeyCode::Char('g') | KeyCode::Home => view.selected = 0,
                    KeyCode::Char('G') | KeyCode::End => view.selected = last,
                    KeyCode::Char('y') => {
                        let text = self
                            .transcript
                            .todos
                            .iter()
                            .map(|item| format!("{} {}", item.status.marker(), item.text))
                            .collect::<Vec<_>>()
                            .join("\n");
                        self.dialog = Dialog::None;
                        self.copy_text(text);
                    }
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.dialog = Dialog::None;
                    }
                    _ => {}
                }
            }
            Dialog::Queue(view) => {
                let queued = self
                    .queue
                    .iter()
                    .filter(|item| item.placement == "queued")
                    .cloned()
                    .collect::<Vec<_>>();
                if queued.is_empty() {
                    self.dialog = Dialog::None;
                    return;
                }
                if view.editing {
                    match key.code {
                        KeyCode::Char(c) => view.draft.push(c),
                        KeyCode::Backspace => {
                            view.draft.pop();
                        }
                        KeyCode::Enter if !view.draft.trim().is_empty() => {
                            let item_id = queued[view.selected].id.clone();
                            let text = std::mem::take(&mut view.draft);
                            view.editing = false;
                            self.queue_action(item_id, json!({ "kind": "edit", "text": text }));
                        }
                        KeyCode::Esc => {
                            view.editing = false;
                            view.draft.clear();
                        }
                        _ => {}
                    }
                    return;
                }
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        view.selected = view.selected.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        view.selected = (view.selected + 1).min(queued.len() - 1);
                    }
                    KeyCode::Char('e') => {
                        if let Some(text) = queued[view.selected].text.clone() {
                            view.editing = true;
                            view.draft = text;
                        }
                    }
                    KeyCode::Char('d') | KeyCode::Delete => {
                        let item_id = queued[view.selected].id.clone();
                        self.queue_action(item_id, json!({ "kind": "remove" }));
                    }
                    KeyCode::Char('s') => {
                        let item_id = queued[view.selected].id.clone();
                        self.queue_action(item_id, json!({ "kind": "steer" }));
                    }
                    KeyCode::Enter => {
                        let item_id = queued[view.selected].id.clone();
                        self.dialog = Dialog::None;
                        self.send_now_item(item_id);
                    }
                    KeyCode::Esc | KeyCode::Char('q') => {
                        self.dialog = Dialog::None;
                    }
                    _ => {}
                }
            }
            Dialog::Subagent(v) => match key.code {
                KeyCode::Up | KeyCode::Char('k') | KeyCode::PageUp => {
                    v.scroll = v.scroll.saturating_add(1);
                }
                KeyCode::Down | KeyCode::Char('j') | KeyCode::PageDown => {
                    v.scroll = v.scroll.saturating_sub(1);
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.dialog = Dialog::None;
                }
                _ => {}
            },
            Dialog::Block(view) => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    view.scroll = view.scroll.saturating_add(1);
                }
                KeyCode::PageUp => {
                    view.scroll = view.scroll.saturating_add(10);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    view.scroll = view.scroll.saturating_sub(1);
                }
                KeyCode::PageDown => {
                    view.scroll = view.scroll.saturating_sub(10);
                }
                KeyCode::Char('r') => {
                    if !view.cell.raw_text.is_empty() {
                        view.cell.raw = !view.cell.raw;
                        view.scroll = 0;
                    }
                }
                KeyCode::Char('y') => {
                    let text = if view.cell.raw {
                        view.cell.raw_text.clone()
                    } else {
                        view.cell.text.clone()
                    };
                    self.copy_text(text);
                }
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                    self.dialog = Dialog::None;
                }
                _ => {}
            },
            Dialog::History(view) => {
                let visible = view.visible.clone();
                match key.code {
                    KeyCode::Up => {
                        view.selected = view.selected.saturating_sub(1);
                    }
                    KeyCode::Down => {
                        view.selected = (view.selected + 1).min(visible.len().saturating_sub(1));
                    }
                    KeyCode::Char(c) => {
                        view.query.push(c);
                        let query = view.query.clone();
                        view.visible = history_matches(&self.history, &query);
                        view.selected = 0;
                    }
                    KeyCode::Backspace => {
                        view.query.pop();
                        let query = view.query.clone();
                        view.visible = history_matches(&self.history, &query);
                        view.selected = 0;
                    }
                    KeyCode::Enter | KeyCode::Tab => {
                        if let Some(index) = visible.get(view.selected).copied() {
                            self.set_input(self.history[index].clone());
                            self.history_cursor = Some(index);
                            self.history_draft.clear();
                            self.focus = Focus::Prompt;
                            self.dialog = Dialog::None;
                        }
                    }
                    KeyCode::Delete => {
                        if let Some(index) = visible.get(view.selected).copied() {
                            self.history.remove(index);
                            let query = view.query.clone();
                            view.visible = history_matches(&self.history, &query);
                            view.selected = view.selected.min(view.visible.len().saturating_sub(1));
                            if view.visible.is_empty() {
                                self.dialog = Dialog::None;
                            }
                        }
                    }
                    KeyCode::Esc => {
                        self.dialog = Dialog::None;
                    }
                    _ => {}
                }
            }
            Dialog::Info(info) => {
                // Grok's /session-info offers both: y copies the whole block,
                // c copies just the session id. A dialog you cannot get text out
                // of is a dead end when mouse reporting owns the selection.
                match key.code {
                    KeyCode::Char('y') => {
                        let text = info
                            .rows
                            .iter()
                            .map(|(k, v)| format!("{k}: {v}"))
                            .collect::<Vec<_>>()
                            .join("\n");
                        self.dialog = Dialog::None;
                        self.copy_text(text);
                    }
                    KeyCode::Char('c') => {
                        let id = info
                            .rows
                            .iter()
                            .find(|(k, _)| k == "session")
                            .map(|(_, v)| v.clone())
                            .unwrap_or_else(|| self.session_id.clone());
                        self.dialog = Dialog::None;
                        self.copy_text(id);
                    }
                    KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                        self.dialog = Dialog::None;
                    }
                    _ => {}
                }
            }
            Dialog::Theme(t) => match key.code {
                KeyCode::Up | KeyCode::Char('k') | KeyCode::Down | KeyCode::Char('j') => {
                    t.selected = 1 - t.selected;
                    self.theme = if t.selected == 0 {
                        // theme_for 重新走终端能力量化，保持与启动主题同一管道，
                        // 而不是把未量化的 DARK/LIGHT 常量直接拍上屏。
                        crate::theme::theme_for("dark")
                    } else {
                        crate::theme::theme_for("light")
                    };
                }
                KeyCode::Enter => {
                    self.dialog = Dialog::None;
                    if let Err(e) = crate::settings::update(&[("theme", self.theme.name)]) {
                        self.set_notice(format!("settings save failed: {e}"));
                    }
                }
                KeyCode::Esc => {
                    self.theme = t.original;
                    self.dialog = Dialog::None;
                }
                _ => {}
            },
            Dialog::Palette(p) => match key.code {
                KeyCode::Up => {
                    p.selected = p.selected.saturating_sub(1);
                }
                KeyCode::Down => {
                    p.selected = (p.selected + 1).min(p.visible.len().saturating_sub(1));
                }
                KeyCode::Char(c) => {
                    p.filter.push(c);
                    p.visible = palette_filter(&p.rows, &p.filter);
                    p.selected = 0;
                }
                KeyCode::Backspace => {
                    p.filter.pop();
                    p.visible = palette_filter(&p.rows, &p.filter);
                    p.selected = 0;
                }
                KeyCode::Enter | KeyCode::Tab => {
                    if let Some(idx) = p.visible.get(p.selected) {
                        let label = p.rows[*idx].label.clone();
                        self.dialog = Dialog::None;
                        match label.as_str() {
                            command if command.starts_with('/') => self.run_command(command),
                            "demo-approval" => self.open_dialog(
                                "demo-approve".into(),
                                "ui/approve",
                                &serde_json::json!({ "toolName": "bash", "reason": "shell command", "input": {"command": "cargo test"}, "options": ["allowed-once", "rejected"] }),
                            ),
                            "demo-question" => self.open_dialog(
                                "demo-ask".into(),
                                "ui/ask-user",
                                &serde_json::json!({ "questions": [
                                    { "id": "q1", "question": "选一个颜色？", "header": "主题", "options": [{"label": "蓝色"}, {"label": "绿色"}] }
                                ] }),
                            ),
                            _ => {}
                        }
                    }
                }
                KeyCode::Esc => {
                    self.dialog = Dialog::None;
                }
                _ => {}
            },
            Dialog::Shortcuts(s) => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    s.selected_section = s.selected_section.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    s.selected_section =
                        (s.selected_section + 1).min(ShortcutSection::ALL.len() - 1);
                }
                KeyCode::Right | KeyCode::Char('l') | KeyCode::Char('e') | KeyCode::Char(' ') => {
                    s.expanded[s.selected_section] = true;
                }
                KeyCode::Left | KeyCode::Char('h') => {
                    s.expanded[s.selected_section] = false;
                }
                KeyCode::Char('/') | KeyCode::Char('f') => {
                    s.filter.clear();
                }
                KeyCode::Char('q') if s.filter.is_empty() => {
                    self.dialog = Dialog::None;
                }
                KeyCode::Char(c) => {
                    s.filter.push(c);
                }
                KeyCode::Backspace => {
                    s.filter.pop();
                }
                KeyCode::Esc => {
                    self.dialog = Dialog::None;
                }
                _ => {}
            },
            Dialog::Rewind(r) => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    r.selected = r.selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    r.selected = (r.selected + 1).min(r.items.len().saturating_sub(1));
                }
                KeyCode::Enter => {
                    if let Some(item) = r.items.get(r.selected) {
                        let session_id = self.session_id.clone();
                        let boundary = item.seq;
                        self.dialog = Dialog::None;
                        self.status = "rewinding".into();
                        let _ = self.cmd_tx.send(Cmd::Rewind {
                            session_id,
                            boundary,
                        });
                    }
                }
                KeyCode::Esc => {
                    self.dialog = Dialog::None;
                }
                _ => {}
            },
            Dialog::Model(m) => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    let visible = m.visible();
                    if !visible.is_empty() {
                        let position = visible
                            .iter()
                            .position(|index| *index == m.selected)
                            .unwrap_or(0);
                        m.selected = visible[position.saturating_sub(1)];
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let visible = m.visible();
                    if !visible.is_empty() {
                        let position = visible
                            .iter()
                            .position(|index| *index == m.selected)
                            .unwrap_or(0);
                        m.selected = visible[(position + 1).min(visible.len().saturating_sub(1))];
                    }
                }
                KeyCode::Char(c) => {
                    m.filter.push(c);
                    if let Some(first) = m.visible().first() {
                        m.selected = *first;
                    }
                }
                KeyCode::Backspace => {
                    m.filter.pop();
                    if let Some(first) = m.visible().first() {
                        m.selected = *first;
                    }
                }
                KeyCode::Enter => {
                    if let Some(model) = m.rows.get(m.selected).cloned() {
                        let efforts = model
                            .reasoning
                            .as_ref()
                            .map(|reasoning| reasoning.efforts.clone())
                            .unwrap_or_default();
                        let selected_effort = model.reasoning.as_ref().and_then(|reasoning| {
                            let selected_id =
                                if model.provider == self.provider && model.id == self.model {
                                    self.reasoning_effort.as_deref()
                                } else {
                                    reasoning.default_effort.as_deref()
                                };
                            selected_id
                                .and_then(|id| {
                                    reasoning.efforts.iter().find(|effort| effort.id == id)
                                })
                                .cloned()
                        });
                        if efforts.is_empty() {
                            self.context_window = model.context_window;
                            self.reasoning_effort_name = None;
                            self.dialog = Dialog::None;
                            self.status = "switching model".into();
                            let _ = self.cmd_tx.send(Cmd::SelectModel {
                                session_id: self.session_id.clone(),
                                provider: model.provider,
                                model: model.id,
                                reasoning_effort: None,
                            });
                        } else {
                            let selected = selected_effort
                                .as_ref()
                                .and_then(|selected| {
                                    efforts.iter().position(|effort| effort.id == selected.id)
                                })
                                .unwrap_or(0);
                            self.dialog = Dialog::Effort(EffortPicker {
                                model,
                                rows: efforts,
                                selected,
                            });
                        }
                    }
                }
                KeyCode::Esc => {
                    self.dialog = Dialog::None;
                }
                _ => {}
            },
            Dialog::Effort(effort) => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    effort.selected = effort.selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    effort.selected =
                        (effort.selected + 1).min(effort.rows.len().saturating_sub(1));
                }
                KeyCode::Enter => {
                    if let Some(selected) = effort.rows.get(effort.selected).cloned() {
                        let model = effort.model.clone();
                        self.context_window = model.context_window;
                        self.reasoning_effort_name = Some(selected.name.clone());
                        self.dialog = Dialog::None;
                        self.status = "switching model".into();
                        let _ = self.cmd_tx.send(Cmd::SelectModel {
                            session_id: self.session_id.clone(),
                            provider: model.provider,
                            model: model.id,
                            reasoning_effort: Some(selected.id),
                        });
                    }
                }
                KeyCode::Esc => {
                    self.dialog = Dialog::None;
                }
                _ => {}
            },
            Dialog::Resume(p) => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    p.selected = p.selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    p.selected = (p.selected + 1).min(p.items.len().saturating_sub(1));
                }
                KeyCode::Enter => {
                    if let Some(item) = p.items.get(p.selected).cloned() {
                        let id = item.id.clone();
                        self.dialog = Dialog::None;
                        self.pending_resume_file = Some(item.file);
                        self.session_id = id.clone();
                        self.status = "resuming".into();
                        let _ = self.cmd_tx.send(Cmd::Load { session_id: id });
                    }
                }

                KeyCode::Esc => {
                    self.dialog = Dialog::None;
                }
                _ => {}
            },
            Dialog::Provider(wizard) => {
                let plain = !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT);
                match key.code {
                    KeyCode::Esc => match (wizard.step, wizard.models_focus) {
                        // Models 步内先退焦点，再退面板。
                        (ProviderStep::Models, ModelsFocus::List) => self.dialog = Dialog::None,
                        (ProviderStep::Models, _) => wizard.models_focus = ModelsFocus::List,
                        _ => self.dialog = Dialog::None,
                    },
                    // 等待桥端结果时屏蔽编辑，只许 Esc。
                    _ if wizard.saving => {}
                    KeyCode::Enter => self.provider_wizard_enter(),
                    KeyCode::BackTab => wizard.back(),
                    // Models 步 Enter 被行展开占用，Tab 承担步进。
                    KeyCode::Tab if wizard.step == ProviderStep::Models => {
                        match wizard.models_focus {
                            ModelsFocus::List => wizard.advance(),
                            _ => wizard.models_focus = ModelsFocus::List,
                        }
                    }
                    // ---- 选项光标的上下移动（Type/Known/Api 词表/Models 列表）----
                    KeyCode::Up | KeyCode::Char('k') => match wizard.step {
                        ProviderStep::Type => wizard.type_sel = wizard.type_sel.saturating_sub(1),
                        ProviderStep::Known => {
                            wizard.catalog_sel = wizard.catalog_sel.saturating_sub(1)
                        }
                        ProviderStep::Api if !wizard.protocols.is_empty() => {
                            wizard.proto_sel = wizard.proto_sel.saturating_sub(1)
                        }
                        ProviderStep::Models => {
                            wizard.model_cursor = wizard.model_cursor.saturating_sub(1);
                            wizard.sync_models_focus();
                        }
                        _ => {}
                    },
                    KeyCode::Down | KeyCode::Char('j') => match wizard.step {
                        ProviderStep::Type => wizard.type_sel = (wizard.type_sel + 1).min(1),
                        ProviderStep::Known => {
                            wizard.catalog_sel =
                                (wizard.catalog_sel + 1).min(wizard.catalog.len().saturating_sub(1))
                        }
                        ProviderStep::Api if !wizard.protocols.is_empty() => {
                            wizard.proto_sel =
                                (wizard.proto_sel + 1).min(wizard.protocols.len().saturating_sub(1))
                        }
                        ProviderStep::Models => {
                            wizard.model_cursor = (wizard.model_cursor + 1)
                                .min(wizard.models.len().saturating_sub(1));
                            wizard.sync_models_focus();
                        }
                        _ => {}
                    },
                    // ---- Models 步的左右与焦点 ----
                    KeyCode::Right if wizard.step == ProviderStep::Models => {
                        match wizard.models_focus {
                            ModelsFocus::List => {
                                if let Some(m) = wizard.models.get_mut(wizard.model_cursor) {
                                    m.open = true;
                                    wizard.detail_col = 0;
                                    wizard.models_focus = ModelsFocus::Detail;
                                }
                            }
                            ModelsFocus::Detail => {
                                wizard.detail_col =
                                    (wizard.detail_col + 1).min(1 + EFFORT_LEVELS.len())
                            }
                            ModelsFocus::Manual => {}
                        }
                    }
                    KeyCode::Left if wizard.step == ProviderStep::Models => {
                        match wizard.models_focus {
                            ModelsFocus::Detail if wizard.detail_col > 0 => wizard.detail_col -= 1,
                            ModelsFocus::Detail => wizard.models_focus = ModelsFocus::List,
                            _ => {}
                        }
                    }
                    KeyCode::Char(' ') if wizard.step == ProviderStep::Models => {
                        match wizard.models_focus {
                            ModelsFocus::List => {
                                if let Some(m) = wizard.models.get_mut(wizard.model_cursor) {
                                    m.included = !m.included;
                                }
                            }
                            ModelsFocus::Detail => wizard_toggle_detail(wizard),
                            ModelsFocus::Manual => wizard.manual.push(' '),
                        }
                    }
                    KeyCode::Char('f')
                        if plain
                            && wizard.step == ProviderStep::Models
                            && wizard.models_focus == ModelsFocus::List
                            && !wizard.fetching =>
                    {
                        // 拉取走桥端（GET {baseURL}/models），anthropic 协议无
                        // 可读列表时桥端会报错，回填到面板。
                        wizard.fetching = true;
                        wizard.error = None;
                        let _ = self.cmd_tx.send(Cmd::FetchModels {
                            api: wizard.api.clone(),
                            base_url: wizard.base_url.clone(),
                            api_key: wizard.api_key.clone(),
                        });
                    }
                    KeyCode::Char('i')
                        if plain
                            && wizard.step == ProviderStep::Models
                            && wizard.models_focus == ModelsFocus::List =>
                    {
                        wizard.models_focus = ModelsFocus::Manual;
                    }
                    KeyCode::Char('o') if plain && wizard.step == ProviderStep::Known => {
                        if let Some(page) = wizard
                            .catalog
                            .get(wizard.catalog_sel)
                            .and_then(|p| p.key_page.clone())
                        {
                            open_url(&page);
                        }
                    }
                    // ---- 文本输入 ----
                    KeyCode::Backspace => {
                        if wizard.step == ProviderStep::Models
                            && wizard.models_focus == ModelsFocus::Manual
                        {
                            wizard.manual.pop();
                        } else if wizard.editing_text() {
                            wizard.draft.pop();
                        }
                    }
                    KeyCode::Char(c) if plain => {
                        if wizard.step == ProviderStep::Models
                            && wizard.models_focus == ModelsFocus::Manual
                        {
                            wizard.manual.push(c);
                        } else if wizard.editing_text() {
                            wizard.draft.push(c);
                        }
                    }
                    _ => {}
                }
            }
            Dialog::ProviderList(view) => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.dialog = Dialog::None;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    view.selected = view.selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    view.selected = (view.selected + 1).min(view.items.len().saturating_sub(1));
                }
                KeyCode::Char('a') => {
                    self.dialog = Dialog::Provider(ProviderWizard::new(
                        self.provider_protocols.clone(),
                        self.catalog_providers.clone(),
                    ));
                }
                KeyCode::Char('e') => {
                    if let Some(item) = view.items.get(view.selected) {
                        self.dialog = Dialog::ProviderKey(ProviderKeyDialog {
                            id: item.id.clone(),
                            draft: String::new(),
                            saving: false,
                            error: None,
                        });
                    }
                }
                KeyCode::Char('d') => {
                    if let Some(item) = view.items.get(view.selected) {
                        self.dialog = Dialog::ProviderRemove(item.id.clone());
                    }
                }
                _ => {}
            },
            Dialog::ProviderKey(dialog) => match key.code {
                KeyCode::Esc => self.reopen_provider_list(),
                _ if dialog.saving => {}
                KeyCode::Enter => {
                    let key = dialog.draft.trim().to_string();
                    if key.is_empty() {
                        dialog.error = Some("key must not be empty".into());
                    } else if self.demo {
                        dialog.error = Some("demo: provider writes need the live bridge".into());
                    } else {
                        dialog.saving = true;
                        dialog.error = None;
                        let _ = self.cmd_tx.send(Cmd::SetProviderKey {
                            id: dialog.id.clone(),
                            api_key: key,
                        });
                    }
                }
                KeyCode::Backspace => {
                    dialog.draft.pop();
                }
                KeyCode::Char(c)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    dialog.draft.push(c);
                }
                _ => {}
            },
            Dialog::ProviderRemove(raw_id) => {
                let id = raw_id.clone();
                match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                        if self.demo {
                            self.dialog = Dialog::None;
                            self.set_notice("demo: provider writes need the live bridge");
                        } else {
                            self.dialog = Dialog::None;
                            self.status = format!("removing {id}");
                            let _ = self.cmd_tx.send(Cmd::RemoveProvider { id: id.clone() });
                        }
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                        self.reopen_provider_list();
                    }
                    _ => {}
                }
            }
            Dialog::None => {}
        }
    }
}

/// Collapse an unmodelled projection value onto one line, WITHOUT truncating.
/// The info card shortens it for display; the row keeps the whole thing so `y`
/// yields something usable rather than a clipped JSON fragment.
fn flatten_value(value: &Value) -> String {
    let text = match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn history_matches(history: &[String], query: &str) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    history
        .iter()
        .enumerate()
        .rev()
        .filter_map(|(index, prompt)| {
            (query.is_empty() || prompt.to_lowercase().contains(&query)).then_some(index)
        })
        .collect()
}

fn queue_preview(text: &str) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = normalized.chars();
    let preview = chars.by_ref().take(200).collect::<String>();
    if chars.next().is_some() {
        format!("{preview}…")
    } else {
        preview
    }
}

fn queue_item_from_value(value: &Value) -> Option<QueueItem> {
    let message = value.get("message")?;
    let blocks = message.get("content")?.as_array()?;
    let mut preview_parts = Vec::with_capacity(blocks.len());
    let mut text = String::new();
    let mut text_only = true;
    for block in blocks {
        if block.get("type").and_then(Value::as_str) == Some("text") {
            let part = block
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            text.push_str(part);
            preview_parts.push(part.to_string());
        } else {
            text_only = false;
            let kind = block
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("content");
            preview_parts.push(format!("[{kind}]"));
        }
    }
    let preview = queue_preview(&preview_parts.join(" "));
    Some(QueueItem {
        id: value.get("id")?.as_str()?.to_string(),
        placement: value.get("placement")?.as_str()?.to_string(),
        preview,
        text: text_only.then_some(text),
    })
}

fn capability_flag(capabilities: Option<&Value>, key: &str) -> bool {
    capabilities
        .and_then(|value| value.get(key))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn palette_filter(rows: &[PaletteRow], query: &str) -> Vec<usize> {
    let q = query.to_lowercase();
    (0..rows.len())
        .filter(|i| {
            q.is_empty()
                || rows[*i].label.to_lowercase().contains(&q)
                || rows[*i].action.to_lowercase().contains(&q)
                || rows[*i].section.to_lowercase().contains(&q)
                || rows[*i]
                    .shortcut
                    .as_deref()
                    .is_some_and(|shortcut| shortcut.to_lowercase().contains(&q))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_app() -> App {
        let (tx, _rx) = std::sync::mpsc::channel::<Cmd>();
        App::new(
            crate::theme::DARK,
            "s1".into(),
            "p".into(),
            "m".into(),
            false,
            tx,
            "/tmp".into(),
        )
    }

    fn key(code: KeyCode, mods: KeyModifiers) -> Event {
        Event::Key(crossterm::event::KeyEvent::new(code, mods))
    }

    fn typed(app: &mut App, text: &str) {
        for c in text.chars() {
            app.handle_key(key(KeyCode::Char(c), KeyModifiers::NONE));
        }
    }

    #[test]
    fn caret_moves_and_edits_mid_draft() {
        let mut app = test_app();
        typed(&mut app, "cargo tst");
        // walk back over "tst" and repair it in place
        for _ in 0..2 {
            app.handle_key(key(KeyCode::Left, KeyModifiers::NONE));
        }
        typed(&mut app, "e");
        assert_eq!(app.input, "cargo test");
        assert_eq!(app.cursor, "cargo te".len());

        app.handle_key(key(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(app.cursor, 0);
        typed(&mut app, "> ");
        assert_eq!(app.input, "> cargo test");
    }

    #[test]
    fn caret_walks_multibyte_text_without_splitting_codepoints() {
        let mut app = test_app();
        typed(&mut app, "你好世界");
        app.handle_key(key(KeyCode::Left, KeyModifiers::NONE));
        app.handle_key(key(KeyCode::Left, KeyModifiers::NONE));
        // two chars back from the end of four 3-byte chars
        assert_eq!(app.cursor, 6);
        typed(&mut app, "X");
        assert_eq!(app.input, "你好X世界");

        app.handle_key(key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(app.input, "你好世界");
        app.handle_key(key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(app.input, "你世界");
    }

    #[test]
    fn readline_kills_and_word_motions() {
        let mut app = test_app();
        typed(&mut app, "fix the auth timeout");

        app.handle_key(key(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(app.input, "fix the auth ");

        app.handle_key(key(KeyCode::Left, KeyModifiers::ALT));
        assert_eq!(app.cursor, "fix the ".len());
        app.handle_key(key(KeyCode::Char('k'), KeyModifiers::CONTROL));
        assert_eq!(app.input, "fix the ");

        app.handle_key(key(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(app.input.is_empty());
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn ctrl_z_undoes_a_kill_and_a_typing_burst() {
        let mut app = test_app();
        typed(&mut app, "keep this");
        app.handle_key(key(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(app.input.is_empty());

        app.handle_key(key(KeyCode::Char('z'), KeyModifiers::CONTROL));
        assert_eq!(app.input, "keep this");
        assert_eq!(app.cursor, "keep this".len());

        // the whole burst is one undo step, not nine
        app.handle_key(key(KeyCode::Char('z'), KeyModifiers::CONTROL));
        assert!(app.input.is_empty());
    }

    #[test]
    fn ctrl_e_ends_the_line_in_the_composer_but_folds_thinking_in_the_scrollback() {
        let mut app = test_app();
        app.transcript
            .push(CellKind::Thinking, "t".to_string(), "reasoning".to_string());
        for cell in &mut app.transcript.cells {
            cell.folded = false;
        }
        typed(&mut app, "abc");
        app.handle_key(key(KeyCode::Home, KeyModifiers::NONE));
        app.handle_key(key(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert_eq!(app.cursor, 3, "Ctrl+E is end-of-line while composing");
        assert!(app
            .transcript
            .cells
            .iter()
            .any(|c| c.kind == CellKind::Thinking && !c.folded));

        app.focus = Focus::Scrollback;
        app.handle_key(key(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert!(
            app.transcript
                .cells
                .iter()
                .all(|c| c.kind != CellKind::Thinking || c.folded),
            "Ctrl+E still folds thinking from the scrollback"
        );
    }

    #[test]
    fn scrollback_autofocus_ignores_control_chords() {
        let mut app = test_app();
        app.transcript
            .push(CellKind::Assistant, String::new(), "hi".to_string());
        app.focus = Focus::Scrollback;
        app.handle_key(key(KeyCode::Char('v'), KeyModifiers::CONTROL));
        assert!(app.input.is_empty(), "Ctrl+V must not type a literal v");
        assert_eq!(app.focus, Focus::Scrollback);

        app.handle_key(key(KeyCode::Char('v'), KeyModifiers::NONE));
        assert_eq!(
            app.input, "v",
            "a bare letter still auto-focuses the composer"
        );
    }

    #[test]
    fn up_down_walk_lines_of_a_multiline_draft_then_fall_back_to_history() {
        let mut app = test_app();
        app.history.push("older".into());
        app.set_input("one\ntwo");

        app.handle_key(key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.input, "one\ntwo", "still editing, not recalling");
        assert_eq!(
            app.cursor, 3,
            "column held at end of the shorter first line"
        );

        // already on the first line, so Up now reaches for history
        app.handle_key(key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.input, "older");
    }

    #[test]
    fn context_rows_report_a_total_and_estimates_without_inviting_a_sum() {
        let mut app = test_app();
        app.projections.apply(
            "contextPressure",
            &json!({"pressureTokens": 90_000, "projectedTokens": 95_000, "contextWindow": 200_000}),
            1,
        );
        app.projections.apply(
            "contextBreakdown",
            &json!({"systemTokens": 2_000, "toolsTokens": 5_000, "messageTokens": 40_000}),
            1,
        );
        let rows = app.context_rows();
        let find = |label: &str| {
            rows.iter()
                .find(|(k, _)| k == label)
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| panic!("missing row {label}: {rows:?}"))
        };

        // the one trustworthy total
        assert_eq!(find("context (next req)"), "95000 / 200000 · 48%");
        assert_eq!(find("context (last req)"), "90000");

        // estimates are marked and listed separately; upstream forbids summing
        // them, and 2000+5000+40000 = 47000 is NOT the total above
        assert_eq!(find("~ system prompt"), "2000");
        assert_eq!(find("~ tool schemas"), "5000");
        assert_eq!(find("~ conversation"), "40000");
        assert!(
            !rows.iter().any(|(k, _)| k.contains("total")),
            "must not present a summed breakdown total: {rows:?}"
        );
    }

    #[test]
    fn context_rows_surface_projections_we_do_not_model_yet() {
        let mut app = test_app();
        app.projections
            .apply("subagentTiming", &json!({"childA": 1234}), 1);
        let rows = app.context_rows();
        assert!(
            rows.iter().any(|(k, _)| k == "subagentTiming"),
            "an unmodelled projection must still be visible: {rows:?}"
        );
    }

    #[test]
    fn mouse_events_that_change_nothing_do_not_request_a_redraw() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut app = test_app();
        app.transcript
            .push(CellKind::Assistant, String::new(), "body".to_string());
        let at = |kind, row| MouseEvent {
            kind,
            column: 10,
            row,
            modifiers: KeyModifiers::NONE,
        };

        // Bare pointer movement is the case that caused the judder: reporting it
        // is now off, but if anything ever sends one it must not draw a frame.
        assert!(
            !app.handle_mouse(at(MouseEventKind::Moved, 5)),
            "pointer movement must not request a frame"
        );
        assert!(!app.handle_mouse(at(MouseEventKind::Drag(MouseButton::Left), 5)));
        assert!(!app.handle_mouse(at(MouseEventKind::Up(MouseButton::Left), 5)));

        // Wheel still does.
        assert!(app.handle_mouse(at(MouseEventKind::ScrollUp, 5)));
        assert!(app.handle_mouse(at(MouseEventKind::ScrollDown, 5)));

        // A click that changes focus does; the same click repeated does not.
        app.composer_top = 30;
        app.focus = Focus::Prompt;
        assert!(
            app.handle_mouse(at(MouseEventKind::Down(MouseButton::Left), 5)),
            "clicking into the scrollback changes focus"
        );
        assert_eq!(app.focus, Focus::Scrollback);
        assert!(
            !app.handle_mouse(at(MouseEventKind::Down(MouseButton::Left), 5)),
            "clicking where focus already is changes nothing"
        );
    }

    #[test]
    fn dialog_clicks_move_selection_and_confirm_like_the_keyboard() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let click = |row: u16| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 15,
            row,
            modifiers: KeyModifiers::NONE,
        };

        // ProviderList：点击只移动选择（Enter 在列表里本无操作），弹窗不关。
        let mut app = test_app();
        app.dialog = Dialog::ProviderList(ProviderListView {
            items: vec![
                ProviderItem {
                    id: "a".into(),
                    key: "none".into(),
                },
                ProviderItem {
                    id: "b".into(),
                    key: "none".into(),
                },
            ],
            selected: 0,
        });
        app.dialog_hits = vec![
            DialogHit::Select {
                row: 5,
                col_start: 10,
                col_end: 30,
                index: 0,
            },
            DialogHit::Select {
                row: 6,
                col_start: 10,
                col_end: 30,
                index: 1,
            },
        ];
        assert!(app.handle_mouse(click(6)));
        let Dialog::ProviderList(view) = &app.dialog else {
            panic!("click on a row must not close the list")
        };
        assert_eq!(view.selected, 1);
        // 未命中任何选项行：忽略，选择不变。
        assert!(!app.handle_mouse(click(9)));
        let Dialog::ProviderList(view) = &app.dialog else {
            panic!("missed click must not close the list")
        };
        assert_eq!(view.selected, 1);
        // 滚轮 = 方向键。
        assert!(app.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            ..click(0)
        }));
        let Dialog::ProviderList(view) = &app.dialog else {
            panic!("list stays open")
        };
        assert_eq!(view.selected, 0);
    }

    #[test]
    fn dialog_click_ask_single_select_answers_and_advances() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let question = |id: &str, options: &[&str], multi_select: bool| Question {
            id: id.into(),
            question: "q?".into(),
            header: String::new(),
            detail: String::new(),
            plan_approve: None,
            options: options.iter().map(|s| s.to_string()).collect(),
            multi_select,
        };
        let mut app = test_app();
        app.dialog = Dialog::Ask(AskDialog {
            request_id: "r".into(),
            questions: vec![
                question("q1", &["x", "y"], false),
                question("q2", &["m"], false),
            ],
            current: 0,
            answers: vec![Vec::new(), Vec::new()],
            cursors: vec![0, 0],
            feedback: String::new(),
            taking_feedback: false,
            detail_scroll: 0,
            custom_text: String::new(),
            taking_text: false,
            parked: false,
        });
        app.dialog_hits = vec![
            DialogHit::Select {
                row: 5,
                col_start: 10,
                col_end: 30,
                index: 0,
            },
            DialogHit::Select {
                row: 6,
                col_start: 10,
                col_end: 30,
                index: 1,
            },
        ];
        let click = |row: u16| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 15,
            row,
            modifiers: KeyModifiers::NONE,
        };
        // 单选：点第 2 项 = 数字键选中 + Enter 跳到下一题。
        assert!(app.handle_mouse(click(6)));
        let Dialog::Ask(d) = &app.dialog else {
            panic!("single-select click advances to the next question")
        };
        assert_eq!(d.answers[0], vec![1]);
        assert_eq!(d.current, 1);
    }

    #[test]
    fn dialog_click_ask_multi_select_toggles_without_advancing() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut app = test_app();
        app.dialog = Dialog::Ask(AskDialog {
            request_id: "r".into(),
            questions: vec![Question {
                id: "q1".into(),
                question: "q?".into(),
                header: String::new(),
                detail: String::new(),
                plan_approve: None,
                options: vec!["x".into(), "y".into()],
                multi_select: true,
            }],
            current: 0,
            answers: vec![Vec::new()],
            cursors: vec![0],
            feedback: String::new(),
            taking_feedback: false,
            detail_scroll: 0,
            custom_text: String::new(),
            taking_text: false,
            parked: false,
        });
        app.dialog_hits = vec![DialogHit::Select {
            row: 5,
            col_start: 10,
            col_end: 30,
            index: 0,
        }];
        let click = || MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 15,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        assert!(app.handle_mouse(click()));
        let Dialog::Ask(d) = &app.dialog else {
            panic!("multi-select click must not submit")
        };
        assert_eq!(d.answers[0], vec![0]);
        assert_eq!(d.current, 0);
        // 再点一次取消勾选。
        assert!(app.handle_mouse(click()));
        let Dialog::Ask(d) = &app.dialog else {
            panic!("still open")
        };
        assert!(d.answers[0].is_empty());
    }

    #[test]
    fn dialog_click_remove_confirm_maps_halves_to_y_and_n() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut app = test_app();
        app.dialog = Dialog::ProviderRemove("deepseek".into());
        app.dialog_hits = vec![
            DialogHit::Key {
                row: 5,
                col_start: 10,
                col_end: 20,
                ch: 'y',
            },
            DialogHit::Key {
                row: 5,
                col_start: 20,
                col_end: 30,
                ch: 'n',
            },
        ];
        let click = |column: u16| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        // 右半 = n：取消（reopen 走 dummy channel，面板关闭）。
        assert!(app.handle_mouse(click(25)));
        assert!(matches!(app.dialog, Dialog::None));
        // 左半 = y：发出 RemoveProvider 并关闭。
        app.dialog = Dialog::ProviderRemove("deepseek".into());
        assert!(app.handle_mouse(click(15)));
        assert!(matches!(app.dialog, Dialog::None));
    }

    #[test]
    fn clicking_composer_text_moves_the_caret_there() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut app = test_app();
        app.set_input("hello world");
        app.cursor = 0;
        // 画过一帧的几何：盒子 30..35，文本区宽 115。
        app.composer_top = 30;
        app.composer_bottom = 35;
        app.composer_inner_width = 115;
        app.focus = Focus::Scrollback;

        // 文本区第一行：x = 边框 1 + 前缀 2 + 第 6 列 → "hello| world"。
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 3 + 6,
            row: 31,
            modifiers: KeyModifiers::NONE,
        };
        assert!(app.handle_mouse(click));
        assert_eq!(app.focus, Focus::Prompt);
        assert_eq!(app.cursor, 6);
        assert_eq!(
            app.input, "hello world",
            "click moves the caret, never edits"
        );

        // 点边框行只聚焦、不动光标。
        let border = MouseEvent { row: 30, ..click };
        assert!(!app.handle_mouse(border));
        assert_eq!(app.cursor, 6);
    }

    #[test]
    fn clicking_the_composer_does_not_yank_the_viewport() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut app = test_app();
        for i in 0..40 {
            app.transcript
                .push(CellKind::Assistant, String::new(), format!("line {i}"));
        }
        app.composer_top = 30;
        app.focus = Focus::Scrollback;
        app.scroll = 25;

        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 4,
            row: 32, // inside the composer
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.focus, Focus::Prompt);
        assert_eq!(
            app.scroll, 25,
            "a click must not scroll the transcript to the bottom"
        );
    }

    #[test]
    fn shift_arrows_scroll_from_the_composer_because_laptops_lack_pageup() {
        let mut app = test_app();
        for i in 0..60 {
            app.transcript
                .push(CellKind::Assistant, String::new(), format!("line {i}"));
        }
        app.focus = Focus::Prompt;
        app.scroll = 0;

        app.handle_key(key(KeyCode::Up, KeyModifiers::SHIFT));
        assert_eq!(app.scroll, 3, "Shift+Up scrolls while typing");
        assert_eq!(app.focus, Focus::Prompt);
        assert!(app.input.is_empty(), "must not land in the draft");

        app.handle_key(key(KeyCode::Down, KeyModifiers::SHIFT));
        assert_eq!(app.scroll, 0);

        // a bare Up is still history recall, not scrolling
        app.history.push("older".into());
        app.handle_key(key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.input, "older");
        assert_eq!(app.scroll, 0);

        // and Shift+Left/Right stay turn navigation in the scrollback
        app.focus = Focus::Scrollback;
        app.scroll = 5;
        app.handle_key(key(KeyCode::Left, KeyModifiers::SHIFT));
        assert_eq!(
            app.scroll, 5,
            "turn navigation must not scroll the viewport"
        );
    }

    #[test]
    fn paging_works_from_the_composer_not_only_the_scrollback() {
        // Regression: PageUp/PageDown used to be gated on scrollback focus, and
        // the mouse wheel was the only way to scroll while the prompt had focus.
        // With mouse reporting off by default that left a resumed session with
        // no way to reach its own replayed history.
        let mut app = test_app();
        for i in 0..60 {
            app.transcript
                .push(CellKind::Assistant, String::new(), format!("line {i}"));
        }
        app.focus = Focus::Prompt;
        app.scroll = 0;

        app.handle_key(key(KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(app.scroll, 10, "PageUp must scroll from the composer");
        assert_eq!(app.focus, Focus::Prompt, "and must not steal focus");
        assert!(!app.follow_selection, "paging detaches from selection");

        app.handle_key(key(KeyCode::PageDown, KeyModifiers::NONE));
        assert_eq!(app.scroll, 0);

        // it must not consume the keys as composer input
        assert!(app.input.is_empty());

        // still works from the scrollback
        app.focus = Focus::Scrollback;
        app.handle_key(key(KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(app.scroll, 10);
    }

    #[test]
    fn info_rows_keep_whole_values_so_copying_them_is_useful() {
        let mut app = test_app();
        let long = serde_json::json!({
            "currentValue": "danger-full-access",
            "options": [{"name": "read-only"}, {"name": "workspace-write"}]
        });
        app.projections.apply("permissions", &long, 1);
        let rows = app.context_rows();
        let value = rows
            .iter()
            .find(|(k, _)| k == "permissions")
            .map(|(_, v)| v.clone())
            .expect("permissions row");

        assert!(
            !value.contains('…'),
            "the stored row must not be truncated — y copies it: {value}"
        );
        assert!(
            value.contains("workspace-write"),
            "the tail of the value has to survive: {value}"
        );
        // and it is still one line
        assert!(!value.contains('\n'));
    }

    #[test]
    fn mouse_toggle_flags_the_event_loop_to_reissue_the_sequence() {
        let mut app = test_app();
        // On by default so the wheel scrolls instead of the terminal turning
        // it into arrow keys (which the composer reads as history recall);
        // dirty at construction so the loop applies whatever the default is.
        assert!(app.mouse_capture);
        assert!(app.mouse_capture_dirty);
        app.mouse_capture_dirty = false;

        app.run_command("/mouse");
        assert!(!app.mouse_capture, "opting out for native selection");
        assert!(app.mouse_capture_dirty);

        app.mouse_capture_dirty = false;
        app.run_command("/mouse");
        assert!(app.mouse_capture);
        assert!(app.mouse_capture_dirty);
    }

    #[test]
    fn info_dialog_can_be_copied_out() {
        let rows = vec![
            ("session".to_string(), "dsh-123".to_string()),
            ("model".to_string(), "p/m".to_string()),
        ];

        // y takes the whole block, one "key: value" per line
        let mut app = test_app();
        app.dialog = Dialog::Info(InfoDialog { rows: rows.clone() });
        app.handle_key(key(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(
            matches!(app.dialog, Dialog::None),
            "copying closes the card"
        );
        assert!(
            app.notice.as_deref().is_some_and(|n| n.contains("copied")),
            "expected a copy confirmation, got {:?}",
            app.notice
        );

        // c takes just the session id
        let mut app = test_app();
        app.dialog = Dialog::Info(InfoDialog { rows });
        app.handle_key(key(KeyCode::Char('c'), KeyModifiers::NONE));
        assert!(matches!(app.dialog, Dialog::None));

        // and the close keys still close without copying
        let mut app = test_app();
        app.dialog = Dialog::Info(InfoDialog { rows: Vec::new() });
        app.handle_key(key(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(matches!(app.dialog, Dialog::None));
    }

    #[test]
    fn context_rows_say_so_when_the_projection_seam_is_missing() {
        // "No rows" used to be indistinguishable from "the harness never mounted
        // dsh-session-projection", which makes a silent failure undiagnosable.
        let mut app = test_app();
        let rows = app.context_rows();
        assert_eq!(
            rows.iter()
                .find(|(k, _)| k == "projections")
                .map(|(_, v)| v.as_str()),
            Some("NOT MOUNTED"),
            "an absent seam must be stated: {rows:?}"
        );

        // mounted but quiet is a different, also-reportable state
        app.capabilities.projections = true;
        let rows = app.context_rows();
        assert_eq!(
            rows.iter()
                .find(|(k, _)| k == "projections")
                .map(|(_, v)| v.as_str()),
            Some("mounted · 0 keys received"),
        );

        // once values flow, name the keys — a count alone could not tell a silent
        // pipeline from one delivering nulls on a fresh session
        app.projections.apply(
            "contextPressure",
            &json!({"projectedTokens": 10, "contextWindow": 100}),
            42,
        );
        let rows = app.context_rows();
        assert_eq!(
            rows.iter()
                .find(|(k, _)| k == "projections")
                .map(|(_, v)| v.as_str()),
            Some("1 keys @seq 42 · contextPressure"),
        );

        // nulls on a fresh session ARE arrivals and must not read as silence
        let mut fresh = test_app();
        fresh.capabilities.projections = true;
        for key in ["goal", "todos"] {
            fresh.projections.apply(key, &Value::Null, 0);
        }
        let rows = fresh.context_rows();
        assert_eq!(
            rows.iter()
                .find(|(k, _)| k == "projections")
                .map(|(_, v)| v.as_str()),
            Some("2 keys @seq 0 · goal todos"),
        );
    }

    #[test]
    fn ctrl_t_opens_todos_instead_of_flipping_the_theme() {
        let mut app = test_app();
        let before = app.theme.name;

        // no snapshot yet: it must not open an empty pane, and must not
        // silently fall back to the old theme toggle
        app.handle_key(key(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert!(matches!(app.dialog, Dialog::None));
        assert_eq!(app.theme.name, before, "Ctrl+T is no longer a theme toggle");

        app.transcript.apply(&json!({
            "type": "tool/call", "seq": 1, "time": 10,
            "data": {"callId": "c1", "name": "todo_write",
                     "arguments": "{\"todos\":[{\"content\":\"a\",\"status\":\"completed\"},{\"content\":\"b\",\"status\":\"in_progress\"}]}"}
        }));
        app.handle_key(key(KeyCode::Char('t'), KeyModifiers::CONTROL));
        let Dialog::Todos(view) = &app.dialog else {
            panic!("expected the todos pane, got {:?}", app.dialog);
        };
        assert_eq!(view.selected, 1, "opens on the in-progress row");
        assert_eq!(app.theme.name, before);

        app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(app.dialog, Dialog::None));
    }

    #[test]
    fn shortcut_panel_opens_and_mode_cycles() {
        let mut app = test_app();
        app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::CONTROL,
        )));
        assert!(matches!(app.dialog, Dialog::Shortcuts(_)));

        app.dialog = Dialog::None;
        app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::BackTab,
            KeyModifiers::SHIFT,
        )));
        assert_eq!(app.permission_mode, PermissionMode::Plan);
        app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::BackTab,
            KeyModifiers::SHIFT,
        )));
        assert_eq!(app.permission_mode, PermissionMode::AlwaysApprove);
    }

    #[test]
    fn mode_cycle_drives_plan_state_and_matching_permission_preset() {
        let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
        let mut app = App::new(
            crate::theme::DARK,
            "s1".into(),
            "p".into(),
            "m".into(),
            false,
            tx,
            "/tmp".into(),
        );
        app.permission_presets = vec![
            "read-only".into(),
            "workspace-write".into(),
            "danger-full-access".into(),
        ];

        app.cycle_permission_mode();
        let Cmd::SetMode { plan, preset, .. } = rx.recv().unwrap() else {
            panic!("expected plan mode command");
        };
        assert!(plan);
        assert_eq!(preset, "read-only");

        app.cycle_permission_mode();
        let Cmd::SetMode { plan, preset, .. } = rx.recv().unwrap() else {
            panic!("expected always-approve mode command");
        };
        assert!(!plan);
        assert_eq!(preset, "danger-full-access");

        app.cycle_permission_mode();
        let Cmd::SetMode { plan, preset, .. } = rx.recv().unwrap() else {
            panic!("expected normal mode command");
        };
        assert!(!plan);
        assert_eq!(preset, "workspace-write");
    }

    #[test]
    fn session_shortcuts_execute_without_typing_commands() {
        let mut app = test_app();
        app.transcript
            .push(CellKind::Assistant, String::new(), "done");
        // Ctrl+N discards the conversation, so the first press only arms it.
        app.handle_key(key(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert!(
            !app.transcript.is_empty(),
            "a single Ctrl+N must not wipe the session"
        );
        app.handle_key(key(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert!(app.transcript.is_empty());
    }

    #[test]
    fn an_expired_confirm_prompt_clears_itself_on_tick() {
        let mut app = test_app();
        app.handle_key(key(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert_eq!(app.notice.as_deref(), Some("press again to quit"));

        // still inside the window: the prompt stays put
        app.tick();
        assert_eq!(app.notice.as_deref(), Some("press again to quit"));

        app.confirm = Some((
            Confirm::Quit,
            Instant::now() - std::time::Duration::from_millis(CONFIRM_MS as u64 + 50),
        ));
        app.tick();
        assert!(app.notice.is_none(), "stale confirm prompt must disappear");
        assert!(app.confirm.is_none());
        assert!(!app.quit);

        // an unrelated notice must survive a tick
        app.notice = Some("theme: light".into());
        app.tick();
        assert_eq!(app.notice.as_deref(), Some("theme: light"));
    }

    #[test]
    fn quit_needs_a_second_press_and_a_stale_arm_expires() {
        let mut app = test_app();
        app.handle_key(key(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert!(!app.quit, "one Ctrl+Q must not drop the draft on the floor");
        app.handle_key(key(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert!(app.quit);

        // an arm that timed out must not be cashed in later
        let mut app = test_app();
        app.handle_key(key(KeyCode::Char('q'), KeyModifiers::CONTROL));
        app.confirm = Some((
            Confirm::Quit,
            Instant::now() - std::time::Duration::from_millis(CONFIRM_MS as u64 + 200),
        ));
        app.handle_key(key(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert!(!app.quit, "expired arm should re-arm, not quit");

        // and a different action cannot satisfy the other's arm
        let mut app = test_app();
        app.handle_key(key(KeyCode::Char('n'), KeyModifiers::CONTROL));
        app.handle_key(key(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert!(!app.quit, "Ctrl+N's arm must not authorise a quit");
    }

    #[test]
    fn ctrl_c_quits_only_when_idle_and_the_composer_is_empty() {
        // 空闲 + 空输入：双按退出（与 Ctrl+Q 共用一条确认臂）。
        let mut app = test_app();
        app.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!app.quit, "a single Ctrl+C only arms the quit");
        app.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.quit);

        // 有草稿时先清草稿，不退出、不占用确认臂。
        let mut app = test_app();
        app.input = "draft".into();
        app.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!app.quit);
        assert!(app.input.is_empty(), "the first Ctrl+C clears the draft");
        app.input = "again".into();
        app.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(
            !app.quit,
            "clearing a draft must not double as the second press"
        );
        assert!(app.input.is_empty());
    }

    #[test]
    fn transient_notices_fade_but_confirm_prompts_survive_the_ttl() {
        let mut app = test_app();
        app.set_notice("copied");
        app.tick();
        assert_eq!(app.notice.as_deref(), Some("copied"), "fresh notice stays");
        app.notice_at =
            Some(Instant::now() - std::time::Duration::from_millis(NOTICE_TTL_MS as u64 + 100));
        app.tick();
        assert_eq!(app.notice, None, "stale notice fades");

        // Confirm prompts die with their arm, not the clock.
        let mut app = test_app();
        app.arm(Confirm::Quit);
        app.set_notice("press again to quit");
        app.notice_at =
            Some(Instant::now() - std::time::Duration::from_millis(NOTICE_TTL_MS as u64 + 100));
        app.tick();
        assert_eq!(
            app.notice.as_deref(),
            Some("press again to quit"),
            "confirm prompt is not eaten by the TTL"
        );
    }

    #[test]
    fn empty_ask_user_request_fails_closed_instead_of_panicking() {
        let mut app = test_app();
        app.open_dialog("req-1".into(), "ui/ask-user", &json!({ "questions": [] }));
        assert!(
            matches!(app.dialog, Dialog::None),
            "an empty questions array must not open a card"
        );
    }

    #[test]
    fn ctrl_d_scrolls_in_the_scrollback_but_quits_from_the_composer() {
        let mut app = test_app();
        app.transcript
            .push(CellKind::Assistant, String::new(), "body");
        app.focus = Focus::Scrollback;
        app.handle_key(key(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert!(!app.quit, "Ctrl+D pages the scrollback, it does not quit");

        app.focus = Focus::Prompt;
        app.handle_key(key(KeyCode::Char('d'), KeyModifiers::CONTROL));
        app.handle_key(key(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert!(app.quit, "from the composer Ctrl+D is the quit alias");
    }

    #[test]
    fn shift_navigation_jumps_between_turns_and_replies() {
        let mut app = test_app();
        for (kind, text) in [
            (CellKind::User, "ask one"),
            (CellKind::Assistant, "reply one"),
            (CellKind::Tool, "tool"),
            (CellKind::User, "ask two"),
            (CellKind::Assistant, "reply two"),
        ] {
            app.transcript.push(kind, String::new(), text.to_string());
        }
        app.focus = Focus::Scrollback;
        app.transcript.selected = Some(4);

        app.handle_key(key(KeyCode::Char('H'), KeyModifiers::SHIFT));
        assert_eq!(app.transcript.selected, Some(3), "previous turn");
        app.handle_key(key(KeyCode::Char('H'), KeyModifiers::SHIFT));
        assert_eq!(app.transcript.selected, Some(0), "the turn before that");
        // no earlier turn: stay put rather than wrap
        app.handle_key(key(KeyCode::Char('H'), KeyModifiers::SHIFT));
        assert_eq!(app.transcript.selected, Some(0));

        app.handle_key(key(KeyCode::Char('J'), KeyModifiers::SHIFT));
        assert_eq!(app.transcript.selected, Some(1), "next assistant reply");
        app.handle_key(key(KeyCode::Char('J'), KeyModifiers::SHIFT));
        assert_eq!(app.transcript.selected, Some(4));
    }

    #[test]
    fn shift_e_toggles_every_fold_at_once() {
        let mut app = test_app();
        app.transcript
            .push(CellKind::Tool, "a".to_string(), "x".to_string());
        app.transcript
            .push(CellKind::Tool, "b".to_string(), "y".to_string());
        app.transcript.cells[0].folded = false;
        app.transcript.cells[1].folded = true;
        app.focus = Focus::Scrollback;

        // something is open, so collapse everything
        app.handle_key(key(KeyCode::Char('E'), KeyModifiers::SHIFT));
        assert!(app.transcript.cells.iter().all(|c| c.folded));
        // now everything is shut, so expand
        app.handle_key(key(KeyCode::Char('E'), KeyModifiers::SHIFT));
        assert!(app.transcript.cells.iter().all(|c| !c.folded));
    }

    #[test]
    fn ctrl_jk_and_ud_scroll_without_moving_the_selection() {
        let mut app = test_app();
        for i in 0..40 {
            app.transcript
                .push(CellKind::Assistant, String::new(), format!("line {i}"));
        }
        app.focus = Focus::Scrollback;
        app.transcript.selected = Some(20);
        app.scroll = 20;

        app.handle_key(key(KeyCode::Char('k'), KeyModifiers::CONTROL));
        assert_eq!(app.scroll, 21, "Ctrl+K scrolls up one line");
        assert_eq!(app.transcript.selected, Some(20), "selection unchanged");

        app.handle_key(key(KeyCode::Char('j'), KeyModifiers::CONTROL));
        assert_eq!(app.scroll, 20, "Ctrl+J scrolls back down");

        app.handle_key(key(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(app.scroll, 30, "Ctrl+U is half a page up");
        assert_eq!(app.transcript.selected, Some(20));
    }

    #[test]
    fn ctrl_f_opens_the_block_viewer_but_a_bare_f_types() {
        let mut app = test_app();
        app.transcript
            .push(CellKind::Tool, "bash".to_string(), "output".to_string());
        app.focus = Focus::Scrollback;
        app.transcript.selected = Some(0);

        app.handle_key(key(KeyCode::Char('f'), KeyModifiers::CONTROL));
        assert!(
            matches!(app.dialog, Dialog::Block(_)),
            "Ctrl+F opens the viewer, got {:?}",
            app.dialog
        );

        app.dialog = Dialog::None;
        app.focus = Focus::Scrollback;
        app.handle_key(key(KeyCode::Char('f'), KeyModifiers::NONE));
        assert_eq!(app.input, "f", "a bare f still auto-focuses and types");
    }

    #[test]
    fn palette_filters_labels_and_actions() {
        let rows = vec![
            PaletteRow {
                label: "/resume".into(),
                action: "Resume Session".into(),
                shortcut: Some("Ctrl+S".into()),
                section: "Session",
            },
            PaletteRow {
                label: "/model".into(),
                action: "Switch Model".into(),
                shortcut: Some("Ctrl+M".into()),
                section: "Model & Input",
            },
        ];
        assert_eq!(palette_filter(&rows, "model"), vec![1]);
        assert_eq!(palette_filter(&rows, "resume"), vec![0]);
        assert_eq!(palette_filter(&rows, "ctrl+s"), vec![0]);
        assert_eq!(palette_filter(&rows, "session"), vec![0]);
        assert_eq!(palette_filter(&rows, "").len(), 2);
    }

    #[test]
    fn bracketed_paste_lands_in_composer() {
        let mut app = test_app();
        app.handle_key(Event::Paste("你好世界".into()));
        assert_eq!(app.input, "你好世界");
        assert_eq!(app.focus, Focus::Prompt);
    }

    #[test]
    fn child_status_does_not_override_active_session() {
        let mut app = test_app();
        app.handle(AppEvent::Rpc {
            method: "session.status".into(),
            params: json!({"sessionId": "child", "status": "running"}),
        });
        assert_eq!(app.state, RunState::Idle);

        app.handle(AppEvent::Rpc {
            method: "session.status".into(),
            params: json!({"sessionId": "s1", "status": "running"}),
        });
        assert_eq!(app.state, RunState::Running);
    }

    #[test]
    fn subagent_finish_extracts_content_blocks() {
        let mut app = test_app();
        app.handle(AppEvent::Rpc {
            method: "subagent.started".into(),
            params: json!({"parentSessionId": "s1", "childSessionId": "child"}),
        });
        app.handle(AppEvent::Rpc {
            method: "subagent.finished".into(),
            params: json!({
                "childSessionId": "child",
                "status": "ok",
                "lastAssistantMessage": [{"type": "text", "text": "done"}]
            }),
        });

        let cell = app
            .transcript
            .cells
            .iter()
            .find(|cell| cell.link.as_deref() == Some("child"))
            .unwrap();
        assert_eq!(cell.text, "ok: done");
    }
    #[test]
    fn ask_enter_commits_focused_option() {
        let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
        let mut app = App::new(
            crate::theme::DARK,
            "s1".into(),
            "p".into(),
            "m".into(),
            false,
            tx,
            "/tmp".into(),
        );
        app.open_dialog(
            "request-1".into(),
            "ui/ask-user",
            &json!({
                "questions": [{
                    "id": "continue",
                    "question": "是否继续？",
                    "options": [{"label": "继续"}, {"label": "停止"}]
                }]
            }),
        );

        app.dialog_key(crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ));

        let Cmd::Respond { id, result } = rx.recv().unwrap() else {
            panic!("expected a dialog response");
        };
        assert_eq!(id, "request-1");
        assert_eq!(
            result,
            json!({"answers": [{"id": "continue", "selected": ["继续"]}]})
        );
    }
    #[test]
    fn history_search_refills_prompt_for_editing() {
        let mut app = test_app();
        app.history = vec![
            "fix authentication timeout".into(),
            "write release notes".into(),
            "inspect authentication logs".into(),
        ];

        app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('r'),
            KeyModifiers::CONTROL,
        )));
        for c in "auth".chars() {
            app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Char(c),
                KeyModifiers::NONE,
            )));
        }
        let Dialog::History(view) = &app.dialog else {
            panic!("expected history search");
        };
        assert_eq!(view.visible, vec![2, 0]);

        app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )));
        app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        assert_eq!(app.input, "fix authentication timeout");
        assert_eq!(app.history_cursor, Some(0));
        assert!(matches!(app.dialog, Dialog::None));

        app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('!'),
            KeyModifiers::NONE,
        )));
        assert_eq!(app.input, "fix authentication timeout!");
        assert_eq!(app.history_cursor, None);
    }

    #[test]
    fn ask_arrow_moves_cursor_before_submit() {
        let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
        let mut app = App::new(
            crate::theme::DARK,
            "s1".into(),
            "p".into(),
            "m".into(),
            false,
            tx,
            "/tmp".into(),
        );
        app.open_dialog(
            "request-2".into(),
            "ui/ask-user",
            &json!({
                "questions": [{
                    "id": "continue",
                    "question": "是否继续？",
                    "options": [{"label": "继续"}, {"label": "停止"}]
                }]
            }),
        );
        app.dialog_key(crossterm::event::KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        ));
        app.dialog_key(crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ));

        let Cmd::Respond { result, .. } = rx.recv().unwrap() else {
            panic!("expected a dialog response");
        };
        assert_eq!(
            result,
            json!({"answers": [{"id": "continue", "selected": ["停止"]}]})
        );
    }
    #[test]
    fn ask_escape_parks_without_responding_and_tab_restores_focus() {
        let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
        let mut app = App::new(
            crate::theme::DARK,
            "s1".into(),
            "p".into(),
            "m".into(),
            false,
            tx,
            "/tmp".into(),
        );
        app.open_dialog(
            "request-park".into(),
            "ui/ask-user",
            &json!({
                "questions": [{
                    "id": "continue",
                    "question": "是否继续？",
                    "options": [{"label": "继续"}, {"label": "停止"}]
                }]
            }),
        );

        app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        )));
        let Dialog::Ask(dialog) = &app.dialog else {
            panic!("expected parked question card");
        };
        assert!(dialog.parked);
        assert_eq!(app.focus, Focus::Scrollback);
        assert!(rx.try_recv().is_err());

        app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Tab,
            KeyModifiers::NONE,
        )));
        let Dialog::Ask(dialog) = &app.dialog else {
            panic!("expected focused question card");
        };
        assert!(!dialog.parked);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn multiline_mode_uses_enter_for_newline_and_alt_enter_for_send() {
        let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
        let mut app = App::new(
            crate::theme::DARK,
            "s1".into(),
            "p".into(),
            "m".into(),
            false,
            tx,
            "/tmp".into(),
        );
        app.set_input("first");

        app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('m'),
            KeyModifiers::CONTROL,
        )));
        app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('s'),
            KeyModifiers::NONE,
        )));
        assert_eq!(app.input, "first\ns");

        app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::ALT,
        )));
        let Cmd::Prompt { text, .. } = rx.recv().unwrap() else {
            panic!("expected prompt command");
        };
        assert_eq!(text, "first\ns");
    }

    #[test]
    fn prompt_history_walks_both_directions_and_restores_draft() {
        let mut app = test_app();
        app.history = vec!["one".into(), "two".into()];
        app.set_input("draft");

        for code in [KeyCode::Up, KeyCode::Up, KeyCode::Down, KeyCode::Down] {
            app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
                code,
                KeyModifiers::NONE,
            )));
        }

        assert_eq!(app.input, "draft");
        assert_eq!(app.history_cursor, None);
    }

    #[test]
    fn scrollback_fold_keys_are_directional_and_g_jumps_to_top() {
        let mut app = test_app();
        app.transcript.push(CellKind::Tool, "first", "first output");
        app.transcript
            .push(CellKind::Tool, "second", "second output");
        app.focus = Focus::Scrollback;

        app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('g'),
            KeyModifiers::NONE,
        )));
        assert_eq!(app.transcript.selected, Some(0));
        app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Right,
            KeyModifiers::NONE,
        )));
        assert!(!app.transcript.cells[0].folded);
        app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Left,
            KeyModifiers::NONE,
        )));
        assert!(app.transcript.cells[0].folded);
    }

    #[test]
    fn catalog_model_choice_requires_effort_then_submits_complete_selection() {
        let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
        let mut app = App::new(
            crate::theme::DARK,
            "session".into(),
            "old-provider".into(),
            "old-model".into(),
            false,
            tx,
            "/tmp".into(),
        );
        // 模拟 /model 路径：用户主动拉目录，结果到达才弹选择器。
        app.catalog_for_picker = true;
        app.handle(AppEvent::Rpc {
            method: "tui/catalog-result".into(),
            params: json!({
                "permissionPresets": [],
                "current": {
                    "provider": "asxs",
                    "model": "gpt-5.6-sol",
                    "reasoningEffort": "low"
                },
                "models": [{
                    "provider": "asxs",
                    "id": "gpt-5.6-sol",
                    "name": "GPT 5.6 Sol",
                    "description": "coding model",
                    "contextWindow": 400000,
                    "reasoning": {
                        "efforts": [
                            {"id": "low", "name": "Low"},
                            {"id": "high", "name": "High"}
                        ],
                        "defaultEffort": "high"
                    }
                }]
            }),
        });

        assert_eq!(app.reasoning_effort_name.as_deref(), Some("Low"));
        app.dialog_key(crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ));
        let Dialog::Effort(effort) = &app.dialog else {
            panic!("expected effort picker");
        };
        assert_eq!(effort.selected, 0);
        assert_eq!(effort.rows[effort.selected].id, "low");
        app.dialog_key(crossterm::event::KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        ));
        app.dialog_key(crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ));

        let Cmd::SelectModel {
            session_id,
            provider,
            model,
            reasoning_effort,
        } = rx.recv().unwrap()
        else {
            panic!("expected model selection");
        };
        assert_eq!(session_id, "session");
        assert_eq!(provider, "asxs");
        assert_eq!(model, "gpt-5.6-sol");
        assert_eq!(reasoning_effort.as_deref(), Some("high"));
        assert_eq!(app.reasoning_effort_name.as_deref(), Some("High"));
        assert_eq!(app.context_window, Some(400000));
    }

    #[test]
    fn startup_catalog_fetch_updates_state_without_opening_the_picker() {
        let (tx, _rx) = std::sync::mpsc::channel::<Cmd>();
        let mut app = App::new(
            crate::theme::DARK,
            "session".into(),
            "old-provider".into(),
            "old-model".into(),
            false,
            tx,
            "/tmp".into(),
        );
        app.handle(AppEvent::Rpc {
            method: "tui/catalog-result".into(),
            params: json!({
                "permissionPresets": [],
                "current": { "provider": "asxs", "model": "gpt-5.6-sol" },
                "models": [{ "provider": "asxs", "id": "gpt-5.6-sol", "name": "GPT 5.6 Sol" }]
            }),
        });
        assert!(
            matches!(app.dialog, Dialog::None),
            "启动时的目录拉取（tui/ready）不得弹模型选择器"
        );
        assert!(app.catalog_loaded, "但 capabilities/presets 数据要照常更新");
    }
    #[test]
    fn plan_review_scrolls_forward_and_feedback_captures_navigation_keys() {
        let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
        let mut app = App::new(
            crate::theme::DARK,
            "s1".into(),
            "p".into(),
            "m".into(),
            false,
            tx,
            "/tmp".into(),
        );
        app.open_dialog(
            "plan-1".into(),
            "ui/ask-user",
            &json!({
                "questions": [{
                    "id": "plan",
                    "question": "Review plan",
                    "detail": "one\ntwo\nthree",
                    "intent": {"approve": "Approve"},
                    "options": [{"label": "Approve"}, {"label": "Request changes"}]
                }]
            }),
        );

        app.plan_review_key(crossterm::event::KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        ));
        let Dialog::Ask(dialog) = &app.dialog else {
            panic!("expected plan review");
        };
        assert_eq!(dialog.detail_scroll, 1);

        app.plan_review_key(crossterm::event::KeyEvent::new(
            KeyCode::Char('s'),
            KeyModifiers::NONE,
        ));
        app.plan_review_key(crossterm::event::KeyEvent::new(
            KeyCode::Char('j'),
            KeyModifiers::NONE,
        ));
        app.plan_review_key(crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ));

        let Cmd::Respond { id, result } = rx.recv().unwrap() else {
            panic!("expected plan response");
        };
        assert_eq!(id, "plan-1");
        assert_eq!(
            result,
            json!({
                "answers": [{
                    "id": "plan",
                    "selected": ["Request changes"],
                    "custom": "j"
                }]
            })
        );
    }
    #[test]
    fn slash_commands_expose_rewind_jobs_and_mode_toggles() {
        let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
        let mut app = App::new(
            crate::theme::DARK,
            "s1".into(),
            "p".into(),
            "m".into(),
            false,
            tx,
            "/tmp".into(),
        );
        app.permission_presets = vec![
            "read-only".into(),
            "workspace-write".into(),
            "danger-full-access".into(),
        ];
        app.transcript.push(CellKind::User, String::new(), "prompt");
        app.transcript
            .turns
            .push(crate::transcript::TurnMarker { seq: 7, cell: 0 });

        app.run_command("/rewind");
        assert!(matches!(app.dialog, Dialog::Rewind(_)));
        app.dialog = Dialog::None;

        app.run_command("/jobs");
        assert!(matches!(rx.recv().unwrap(), Cmd::FetchJobs));

        app.run_command("/plan implement auth");
        let Cmd::SetMode { plan, preset, .. } = rx.recv().unwrap() else {
            panic!("expected plan mode command");
        };
        assert!(plan);
        assert_eq!(preset, "read-only");

        app.run_command("/always-approve");
        let Cmd::SetMode { plan, preset, .. } = rx.recv().unwrap() else {
            panic!("expected always-approve command");
        };
        assert!(!plan);
        assert_eq!(preset, "danger-full-access");

        app.run_command("/always-approve");
        let Cmd::SetMode { plan, preset, .. } = rx.recv().unwrap() else {
            panic!("expected normal mode command");
        };
        assert!(!plan);
        assert_eq!(preset, "workspace-write");
    }

    #[test]
    fn catalog_capabilities_drive_palette_and_harness_commands() {
        let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
        let mut app = App::new(
            crate::theme::DARK,
            "session".into(),
            "provider".into(),
            "model".into(),
            false,
            tx,
            "/tmp".into(),
        );
        app.handle(AppEvent::Rpc {
            method: "tui/catalog-result".into(),
            params: json!({
                "permissionPresets": null,
                "capabilities": {
                    "models": false,
                    "permissions": false,
                    "planMode": false,
                    "compaction": false,
                    "jobs": false,
                    "userQuestions": true,
                    "sessionSearch": true,
                    "commands": true,
                    "tools": true
                },
                "commands": [{
                    "name": "goal",
                    "description": "Manage the active goal",
                    "inputHint": "create|get|complete"
                }],
                "current": {"provider": "provider", "model": "model"},
                "models": []
            }),
        });
        app.open_palette();
        let Dialog::Palette(palette) = &app.dialog else {
            panic!("expected capability palette");
        };
        assert!(palette.rows.iter().any(|row| row.label == "/goal"));
        assert!(!palette.rows.iter().any(|row| row.label == "/jobs"));
        assert!(!palette.rows.iter().any(|row| row.label == "/compact"));
        assert!(!palette.rows.iter().any(|row| row.label == "/model"));

        app.dialog = Dialog::None;
        app.run_command("/goal get");
        let Cmd::ExecuteCommand { session_id, line } = rx.recv().unwrap() else {
            panic!("expected Harness command execution");
        };
        assert_eq!(session_id, "session");
        assert_eq!(line, "/goal get");
    }

    #[test]
    fn slash_opens_filtered_command_menu_and_tab_executes_selection() {
        let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
        let mut app = App::new(
            crate::theme::DARK,
            "s1".into(),
            "p".into(),
            "m".into(),
            false,
            tx,
            "/tmp".into(),
        );

        app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('/'),
            KeyModifiers::NONE,
        )));
        let Dialog::Palette(palette) = &app.dialog else {
            panic!("expected slash command menu");
        };
        assert!(app.input.is_empty());
        assert_eq!(palette.filter, "");

        for c in "jobs".chars() {
            app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Char(c),
                KeyModifiers::NONE,
            )));
        }
        let Dialog::Palette(palette) = &app.dialog else {
            panic!("expected filtered slash command menu");
        };
        assert_eq!(palette.visible.len(), 1);
        assert_eq!(palette.rows[palette.visible[0]].label, "/jobs");

        app.handle_key(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Tab,
            KeyModifiers::NONE,
        )));
        assert!(matches!(rx.recv().unwrap(), Cmd::FetchCatalog { .. }));
        assert!(matches!(rx.recv().unwrap(), Cmd::FetchJobs));
        assert!(matches!(app.dialog, Dialog::None));
    }

    #[test]
    fn provider_id_validation_matches_the_bridge_pattern() {
        for valid in ["a", "openrouter", "my-provider", "a1-b2", "x-y-z"] {
            assert!(valid_provider_id(valid), "{valid} should be valid");
        }
        for invalid in ["", "A", "1a", "-a", "a-", "a--b", "a_b", "a b", "OPENCODE"] {
            assert!(!valid_provider_id(invalid), "{invalid} should be invalid");
        }
    }

    /// 测试辅助：构造带命令通道的 App 并打开 add 向导（含目录预设）。
    fn wizard_app() -> (App, std::sync::mpsc::Receiver<Cmd>) {
        let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
        let mut app = App::new(
            crate::theme::DARK,
            "s1".into(),
            "p".into(),
            "m".into(),
            false,
            tx,
            "/tmp".into(),
        );
        app.catalog_providers = vec![
            CatalogProvider {
                id: "deepseek".into(),
                name: "DeepSeek".into(),
                key_page: Some("https://platform.deepseek.com/api-keys".into()),
            },
            CatalogProvider {
                id: "openai".into(),
                name: "OpenAI".into(),
                key_page: None,
            },
        ];
        app.run_command("/provider add");
        (app, rx)
    }

    fn set_draft(app: &mut App, text: &str) {
        if let Dialog::Provider(w) = &mut app.dialog {
            w.draft = text.to_string();
        }
    }

    /// 走完 custom 分支的连接段，停在 Models 步。
    fn walk_custom_to_models(app: &mut App) {
        // Type：光标移到"自定义"（type_sel = 1）。
        app.handle_key(key(KeyCode::Down, KeyModifiers::NONE));
        app.provider_wizard_enter();
        set_draft(app, "my-api");
        app.provider_wizard_enter();
        set_draft(app, "openai-responses");
        app.provider_wizard_enter();
        set_draft(app, "https://api.example.com/v1");
        app.provider_wizard_enter();
        set_draft(app, "sk-test");
        app.provider_wizard_enter();
    }

    #[test]
    fn provider_wizard_custom_walks_and_emits_rich_save() {
        let (mut app, rx) = wizard_app();
        walk_custom_to_models(&mut app);
        let Dialog::Provider(w) = &app.dialog else {
            panic!("wizard open");
        };
        assert_eq!(w.step, ProviderStep::Models);

        // 手填两个模型，给第二个配多模态 + 思考级别。
        if let Dialog::Provider(w) = &mut app.dialog {
            w.models = vec![
                ModelDraft {
                    id: "model-a".into(),
                    included: true,
                    vision: false,
                    reasoning: false,
                    efforts: vec![],
                    open: false,
                },
                ModelDraft {
                    id: "model-b".into(),
                    included: true,
                    vision: true,
                    reasoning: true,
                    efforts: vec!["low".into(), "high".into(), "ultra".into()],
                    open: false,
                },
            ];
        }
        // Tab 步进到 Confirm，Enter 保存。
        app.handle_key(key(KeyCode::Tab, KeyModifiers::NONE));
        app.provider_wizard_enter();
        let Cmd::SaveProvider { draft } = rx.recv().unwrap() else {
            panic!("expected SaveProvider");
        };
        assert_eq!(draft["id"], "my-api");
        assert_eq!(draft["api"], "openai-responses");
        assert_eq!(draft["baseURL"], "https://api.example.com/v1");
        assert_eq!(draft["apiKey"], "sk-test");
        assert_eq!(
            draft["models"],
            json!([
                { "id": "model-a", "vision": false, "efforts": [] },
                { "id": "model-b", "vision": true, "efforts": ["low", "high", "ultra"] },
            ])
        );
        // 成功：面板关闭、通知、后台刷新目录。
        app.handle(AppEvent::Rpc {
            method: "tui/provider-saved".into(),
            params: json!({ "ok": true, "id": "my-api", "keyRef": "MY_API_API_KEY", "keyStored": true, "keyError": null }),
        });
        assert!(matches!(app.dialog, Dialog::None));
        assert!(app.notice.as_deref().unwrap().contains("my-api added"));
        assert!(matches!(rx.recv().unwrap(), Cmd::FetchCatalog { .. }));
    }

    #[test]
    fn provider_wizard_known_flow_writes_key_only() {
        let (mut app, rx) = wizard_app();
        // Type 默认光标就在"内置目录"，直接 Enter。
        app.provider_wizard_enter();
        let Dialog::Provider(w) = &app.dialog else {
            panic!("wizard open");
        };
        assert_eq!(w.step, ProviderStep::Known);
        // 光标下移到 openai，Enter 选定 → 跳到 ApiKey。
        app.handle_key(key(KeyCode::Down, KeyModifiers::NONE));
        app.provider_wizard_enter();
        let Dialog::Provider(w) = &app.dialog else {
            panic!("wizard open");
        };
        assert_eq!(w.step, ProviderStep::ApiKey);
        assert_eq!(w.id, "openai");
        set_draft(&mut app, "sk-openai");
        app.provider_wizard_enter(); // → Confirm
        app.provider_wizard_enter(); // 保存
        let Cmd::SaveProvider { draft } = rx.recv().unwrap() else {
            panic!("expected SaveProvider");
        };
        // known 形态：只带 key 与 known 标记，协议/URL/模型由目录供给。
        assert_eq!(
            draft,
            json!({ "id": "openai", "apiKey": "sk-openai", "known": true })
        );
    }

    #[test]
    fn provider_wizard_save_failure_stays_in_panel_for_retry() {
        let (mut app, rx) = wizard_app();
        app.provider_wizard_enter(); // Type → Known
        app.provider_wizard_enter(); // Known → ApiKey（deepseek）
        app.provider_wizard_enter(); // ApiKey（空）→ Confirm
        app.provider_wizard_enter(); // 保存
        let _ = rx.recv().unwrap();
        app.handle(AppEvent::Rpc {
            method: "tui/provider-saved".into(),
            params: json!({ "ok": false, "error": "provider already exists: deepseek" }),
        });
        let Dialog::Provider(w) = &app.dialog else {
            panic!("failure must keep the panel open");
        };
        assert!(!w.saving);
        assert_eq!(
            w.error.as_deref(),
            Some("provider already exists: deepseek")
        );
        assert_eq!(w.id, "deepseek");
    }

    #[test]
    fn provider_wizard_custom_requires_a_model() {
        let (mut app, _rx) = wizard_app();
        walk_custom_to_models(&mut app);
        app.handle_key(key(KeyCode::Tab, KeyModifiers::NONE)); // → Confirm
        app.provider_wizard_enter();
        let Dialog::Provider(w) = &app.dialog else {
            panic!("wizard open");
        };
        assert_eq!(w.step, ProviderStep::Confirm);
        assert!(w.error.as_deref().unwrap().contains("at least one model"));
    }

    #[test]
    fn provider_wizard_models_step_fetch_manual_and_detail() {
        let (mut app, rx) = wizard_app();
        walk_custom_to_models(&mut app);
        // f 拉取：发出 FetchModels，事件回填列表。
        app.handle_key(key(KeyCode::Char('f'), KeyModifiers::NONE));
        let Cmd::FetchModels {
            api,
            base_url,
            api_key,
        } = rx.recv().unwrap()
        else {
            panic!("expected FetchModels");
        };
        assert_eq!(api, "openai-responses");
        assert_eq!(base_url, "https://api.example.com/v1");
        assert_eq!(api_key, "sk-test");
        app.handle(AppEvent::Rpc {
            method: "tui/models-fetched".into(),
            params: json!({ "ok": true, "models": ["gpt-a", "gpt-b"] }),
        });
        // i 手填一个，Enter 提交。
        app.handle_key(key(KeyCode::Char('i'), KeyModifiers::NONE));
        if let Dialog::Provider(w) = &mut app.dialog {
            w.manual = "manual-c".into();
        }
        app.provider_wizard_enter();
        let Dialog::Provider(w) = &app.dialog else {
            panic!("wizard open");
        };
        assert_eq!(w.models.len(), 3);
        assert_eq!(w.models[2].id, "manual-c");
        // 拉到的模型默认 included + reasoning 5 级。
        assert!(w.models[0].included);
        assert_eq!(w.models[0].efforts.len(), 5);
        // 手填后光标在 manual-c（models[2]）；先回到 models[0] 再展开。
        app.handle_key(key(KeyCode::Up, KeyModifiers::NONE));
        app.handle_key(key(KeyCode::Up, KeyModifiers::NONE));
        // → 展开第一行进 Detail，Space 切多模态，→→ Space 切 chips。
        app.handle_key(key(KeyCode::Right, KeyModifiers::NONE));
        app.handle_key(key(KeyCode::Char(' '), KeyModifiers::NONE)); // vision on
        app.handle_key(key(KeyCode::Right, KeyModifiers::NONE)); // col 1 = 思考开关
        app.handle_key(key(KeyCode::Right, KeyModifiers::NONE)); // col 2 = low chip
        app.handle_key(key(KeyCode::Char(' '), KeyModifiers::NONE)); // low off
        let Dialog::Provider(w) = &app.dialog else {
            panic!("wizard open");
        };
        assert!(w.models[0].vision);
        assert!(!w.models[0].efforts.iter().any(|e| e == "low"));
        assert!(w.models[0].reasoning, "还有其它级别，思考保持开启");
        // Esc 从 Detail 回 List，再 Esc 关面板。
        app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE));
        let Dialog::Provider(w) = &app.dialog else {
            panic!("still open");
        };
        assert_eq!(w.models_focus, ModelsFocus::List);
    }

    #[test]
    fn provider_wizard_esc_closes_panel_without_side_effects() {
        let (mut app, rx) = wizard_app();
        app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(app.dialog, Dialog::None));
        // catalog 已预置（wizard_app），add 不会再补拉；无任何命令发出。
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn provider_wizard_selects_protocol_from_bridge_vocabulary() {
        let (mut app, _rx) = wizard_app();
        app.handle(AppEvent::Rpc {
            method: "tui/ready".into(),
            params: json!({ "server": { "protocols": ["openai-completions", "openai-responses", "anthropic-messages"] } }),
        });
        app.handle_key(key(KeyCode::Down, KeyModifiers::NONE)); // custom
        app.provider_wizard_enter();
        set_draft(&mut app, "my-api");
        app.provider_wizard_enter();
        let Dialog::Provider(w) = &app.dialog else {
            panic!("wizard open");
        };
        assert_eq!(w.step, ProviderStep::Api);
        assert!(!w.editing_text());
        app.handle_key(key(KeyCode::Down, KeyModifiers::NONE));
        app.provider_wizard_enter();
        let Dialog::Provider(w) = &app.dialog else {
            panic!("wizard advanced");
        };
        assert_eq!(w.api, "openai-responses");
        assert_eq!(w.step, ProviderStep::BaseUrl);
    }

    #[test]
    fn provider_list_opens_dialog_with_key_status_and_catalog() {
        let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
        let mut app = App::new(
            crate::theme::DARK,
            "s1".into(),
            "p".into(),
            "m".into(),
            false,
            tx,
            "/tmp".into(),
        );
        app.run_command("/provider");
        assert!(matches!(rx.recv().unwrap(), Cmd::ListProviders));
        app.handle(AppEvent::Rpc {
            method: "tui/providers".into(),
            params: json!({
                "providers": [
                    { "id": "opencode-go", "name": "opencode-go", "keyRef": "OPENCODE_GO_API_KEY", "key": { "configured": true, "source": "file", "writable": true } },
                    { "id": "deepseek-official", "name": "deepseek-official", "keyRef": "DEEPSEEK_OFFICIAL_API_KEY", "key": { "configured": false, "writable": true } },
                ],
                "protocols": ["openai-responses"],
                "catalogProviders": [ { "id": "deepseek", "name": "DeepSeek", "keyPage": "https://platform.deepseek.com/api-keys" } ],
            }),
        });
        let Dialog::ProviderList(view) = &app.dialog else {
            panic!("expected provider list dialog");
        };
        assert_eq!(view.items.len(), 2);
        assert_eq!(view.items[0].id, "opencode-go");
        assert_eq!(view.items[0].key, "file");
        assert_eq!(view.items[1].key, "none");
        assert_eq!(app.provider_protocols, vec!["openai-responses".to_string()]);
        assert_eq!(app.catalog_providers.len(), 1);
        assert_eq!(app.catalog_providers[0].id, "deepseek");
    }

    #[test]
    fn provider_list_actions_open_sub_dialogs_and_flow() {
        let (tx, rx) = std::sync::mpsc::channel::<Cmd>();
        let mut app = App::new(
            crate::theme::DARK,
            "s1".into(),
            "p".into(),
            "m".into(),
            false,
            tx,
            "/tmp".into(),
        );
        app.dialog = Dialog::ProviderList(ProviderListView {
            items: vec![ProviderItem {
                id: "asxs".into(),
                key: "file".into(),
            }],
            selected: 0,
        });
        // e → key 输入面板，Enter 发 SetProviderKey。
        app.handle_key(key(KeyCode::Char('e'), KeyModifiers::NONE));
        let Dialog::ProviderKey(dialog) = &mut app.dialog else {
            panic!("expected key dialog");
        };
        dialog.draft = "sk-new".into();
        app.handle_key(key(KeyCode::Enter, KeyModifiers::NONE));
        let Cmd::SetProviderKey { id, api_key } = rx.recv().unwrap() else {
            panic!("expected SetProviderKey");
        };
        assert_eq!(id, "asxs");
        assert_eq!(api_key, "sk-new");
        // 成功：回列表（重发 ListProviders）。
        app.handle(AppEvent::Rpc {
            method: "tui/key-saved".into(),
            params: json!({ "ok": true, "id": "asxs" }),
        });
        assert!(matches!(rx.recv().unwrap(), Cmd::ListProviders));
        // d → 删除确认，y 发 RemoveProvider。
        app.dialog = Dialog::ProviderList(ProviderListView {
            items: vec![ProviderItem {
                id: "asxs".into(),
                key: "file".into(),
            }],
            selected: 0,
        });
        app.handle_key(key(KeyCode::Char('d'), KeyModifiers::NONE));
        assert!(matches!(app.dialog, Dialog::ProviderRemove(_)));
        app.handle_key(key(KeyCode::Char('y'), KeyModifiers::NONE));
        let Cmd::RemoveProvider { id } = rx.recv().unwrap() else {
            panic!("expected RemoveProvider");
        };
        assert_eq!(id, "asxs");
        app.handle(AppEvent::Rpc {
            method: "tui/provider-removed".into(),
            params: json!({ "ok": true, "id": "asxs" }),
        });
        assert!(app.notice.as_deref().unwrap().contains("asxs removed"));
    }
}

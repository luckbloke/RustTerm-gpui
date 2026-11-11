use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// AI 提供方。
///
/// - `openai`：OpenAI 兼容协议（`{base}/chat/completions`），也覆盖
///   各类中转网关 / GLM / Kilo 等兼容端点。
/// - `ollama`：本地 Ollama（`{base}/api/chat`），不需要 API Key。
/// - `anthropic`：Anthropic Messages API（`{base}/v1/messages`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AiProvider {
    Openai,
    Ollama,
    Anthropic,
}

impl AiProvider {
    pub const ALL: [AiProvider; 3] =
        [AiProvider::Openai, AiProvider::Ollama, AiProvider::Anthropic];

    /// 下拉框显示名。
    pub fn label(&self) -> &'static str {
        match self {
            AiProvider::Openai => "OpenAI",
            AiProvider::Ollama => "Ollama（本地）",
            AiProvider::Anthropic => "Anthropic",
        }
    }

    /// 持久化用的稳定标识。
    pub fn id(&self) -> &'static str {
        match self {
            AiProvider::Openai => "openai",
            AiProvider::Ollama => "ollama",
            AiProvider::Anthropic => "anthropic",
        }
    }

    pub fn from_id(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "ollama" => AiProvider::Ollama,
            "anthropic" | "claude" => AiProvider::Anthropic,
            _ => AiProvider::Openai,
        }
    }

    /// 该提供方的默认 Base URL。
    pub fn default_base_url(&self) -> &'static str {
        match self {
            AiProvider::Openai => "https://api.openai.com/v1",
            AiProvider::Ollama => "http://localhost:11434",
            AiProvider::Anthropic => "https://api.anthropic.com",
        }
    }

    /// 该提供方的默认模型名。
    pub fn default_model(&self) -> &'static str {
        match self {
            AiProvider::Openai => "gpt-4o-mini",
            AiProvider::Ollama => "qwen2.5:7b",
            AiProvider::Anthropic => "claude-3-5-sonnet-latest",
        }
    }

    /// 是否需要 API Key（Ollama 本地推理不需要）。
    pub fn needs_api_key(&self) -> bool {
        !matches!(self, AiProvider::Ollama)
    }

    /// 切换提供方时是否要重置 Base URL / 模型。
    ///
    /// 用户手动填过中转网关地址时不应被覆盖，因此只有当当前值
    /// 恰好等于「上一个提供方的默认值」时才跟随切换。
    pub fn should_reset_url(&self, current: &str, prev: AiProvider) -> bool {
        current.trim().is_empty() || current.trim() == prev.default_base_url()
    }

    pub fn should_reset_model(&self, current: &str, prev: AiProvider) -> bool {
        current.trim().is_empty() || current.trim() == prev.default_model()
    }
}

/// AI 相关配置（对应「AI 配置」面板的全部字段）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AiSettings {
    /// 提供方：`openai` / `ollama` / `anthropic`
    #[serde(default = "default_ai_provider")]
    pub provider: String,
    /// API Key（仅保存在本地；Ollama 不需要）
    #[serde(default)]
    pub api_key: String,
    /// 接口根地址
    #[serde(default = "default_ai_base")]
    pub base_url: String,
    /// 模型名
    #[serde(default = "default_ai_model")]
    pub model: String,
    /// 单次回复最大 Token 数
    #[serde(default = "default_ai_max_tokens")]
    pub max_tokens: u32,
    /// 采样温度
    #[serde(default = "default_ai_temperature")]
    pub temperature: f32,
    /// 只读模式：为 true 时禁止把 AI 给出的写命令发到终端
    #[serde(default)]
    pub read_only: bool,
    /// 对话历史最多保留多少条
    #[serde(default = "default_ai_max_history")]
    pub max_history: usize,
    /// 每次附带最近多少行终端上下文
    #[serde(default = "default_ai_context_lines")]
    pub context_lines: usize,
    /// 上下文中单行最多保留多少字符（超出截断）
    #[serde(default = "default_ai_context_max_line_len")]
    pub context_max_line_len: usize,
}

fn default_ai_provider() -> String {
    "openai".to_string()
}
fn default_ai_model() -> String {
    AiProvider::Openai.default_model().to_string()
}
fn default_ai_base() -> String {
    AiProvider::Openai.default_base_url().to_string()
}
fn default_ai_max_tokens() -> u32 {
    2048
}
fn default_ai_temperature() -> f32 {
    0.3
}
fn default_ai_max_history() -> usize {
    20
}
fn default_ai_context_lines() -> usize {
    50
}
fn default_ai_context_max_line_len() -> usize {
    200
}

impl Default for AiSettings {
    fn default() -> Self {
        Self {
            provider: default_ai_provider(),
            api_key: String::new(),
            base_url: default_ai_base(),
            model: default_ai_model(),
            max_tokens: default_ai_max_tokens(),
            temperature: default_ai_temperature(),
            read_only: false,
            max_history: default_ai_max_history(),
            context_lines: default_ai_context_lines(),
            context_max_line_len: default_ai_context_max_line_len(),
        }
    }
}

impl AiSettings {
    /// 当前提供方枚举。
    pub fn provider(&self) -> AiProvider {
        AiProvider::from_id(&self.provider)
    }

    /// Base URL（去掉尾部斜杠）。
    pub fn trimmed_base(&self) -> String {
        self.base_url.trim().trim_end_matches('/').to_string()
    }

    /// 接口根地址是否为空（空表示未配置）。
    pub fn has_endpoint(&self) -> bool {
        !self.trimmed_base().is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub font_size: f32,
    pub font_family: String,
    pub theme: String,
    pub language: String,
    pub show_status_bar: bool,
    pub keep_alive_interval: u64,
    pub enable_key_macros: bool,
    /// AI 助手配置（提供方 / 模型 / 上下文策略）
    #[serde(default)]
    pub ai: AiSettings,
    /// ZMODEM（rz/sz）自动收发：远端跑 `sz` 时自动接收、跑 `rz` 时弹文件选择器。
    ///
    /// 关掉后终端流里出现 ZMODEM 头也当普通输出显示（排障用：万一某个程序
    /// 的输出里真的带 `**\x18B00`，可以一键停掉自动接管）。
    #[serde(default = "default_true")]
    pub zmodem_enabled: bool,
    /// ZMODEM 接收文件的落盘目录；None / 空串表示按系统下载目录回退。
    #[serde(default)]
    pub zmodem_download_dir: Option<String>,
    /// 内置 X server 的输入协议：Xi2（默认）/ Core（核心事件，兼容排障）。
    /// 老配置无此字段时必须反序列化为 Xi2（`#[serde(default)]`）。
    #[serde(default)]
    pub xinput_mode: XInputMode,
}

fn default_true() -> bool {
    true
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            font_size: 14.0,
            font_family: "monospace".to_string(),
            theme: "dark".to_string(),
            language: "zh-CN".to_string(),
            show_status_bar: true,
            keep_alive_interval: 30,
            enable_key_macros: true,
            ai: AiSettings::default(),
            zmodem_enabled: true,
            zmodem_download_dir: None,
            xinput_mode: XInputMode::Xi2,
        }
    }
}

impl AppConfig {
    pub fn config_path() -> Option<PathBuf> {
        dirs::config_dir().map(|p| p.join("rustterm-gpui").join("config.json"))
    }

    /// 旧版（egui）配置路径，仅用于首次运行时迁移读取。
    pub fn legacy_path() -> Option<PathBuf> {
        dirs::config_dir().map(|p| p.join("rustterm-egui").join("config.json"))
    }

    /// 回退路径：程序目录旁的 `data/config.json`（主配置目录被安全软件
    /// 拦截写入时用，与 sessions.json 的回退策略一致）。
    pub fn fallback_path() -> Option<PathBuf> {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("data").join("config.json")))
    }

    fn write_json(path: &PathBuf, data: &str) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("{} 创建失败: {e}", parent.display()))?;
        }
        fs::write(path, data)
            .map_err(|e| format!("{} 写入失败: {e}", path.display()))
    }

    pub fn load() -> Self {
        let cfg = Self::load_inner();
        // 心跳间隔做成进程级全局量，见 `keep_alive_interval` 的说明
        set_keep_alive_interval(cfg.keep_alive_interval);
        // ZMODEM 同理：窗格的创建入口很多（本地 / SSH / Telnet / 命令 / 分屏
        // 复制），逐个把配置传进 TermPane 太容易漏；而且改完配置要求已经
        // 在跑的窗格立即生效，所以也落成进程级全局量。
        set_zmodem(cfg.zmodem_enabled, cfg.zmodem_download_dir.clone());
        // 输入协议同理：query_extension 在任意握手线程上查表，走进程级全局量
        set_xinput_mode(cfg.xinput_mode);
        cfg
    }

    fn load_inner() -> Self {
        let Some(path) = Self::config_path() else {
            return Self::default();
        };
        let data = match fs::read_to_string(&path) {
            Ok(d) => d,
            Err(_) => {
                // 主路径不存在时依次尝试：旧版 egui 目录（一次性迁移）→ 回退路径
                if let Some(lg) = Self::legacy_path() {
                    if let Ok(d) = fs::read_to_string(&lg) {
                        return Self::parse_or_legacy(&d);
                    }
                }
                if let Some(fb) = Self::fallback_path() {
                    if let Ok(d) = fs::read_to_string(&fb) {
                        return Self::parse_or_legacy(&d);
                    }
                }
                return Self::default();
            }
        };
        Self::parse_or_legacy(&data)
    }

    fn parse_or_legacy(data: &str) -> Self {
        match serde_json::from_str::<Self>(data) {
            Ok(cfg) => cfg,
            Err(_) => {
                // 兼容旧版扁平字段（ai_api_key / ai_model / ai_api_base）。
                if let Some(cfg) = Self::load_legacy(data) {
                    return cfg;
                }
                Self::default()
            }
        }
    }

    /// 读取旧格式配置并迁移到新的 `ai` 子结构。
    fn load_legacy(data: &str) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(data).ok()?;
        let obj = v.as_object()?;
        let mut cfg = Self::default();
        if let Some(n) = obj.get("font_size").and_then(|x| x.as_f64()) {
            cfg.font_size = n as f32;
        }
        if let Some(s) = obj.get("font_family").and_then(|x| x.as_str()) {
            cfg.font_family = s.to_string();
        }
        if let Some(s) = obj.get("theme").and_then(|x| x.as_str()) {
            cfg.theme = s.to_string();
        }
        if let Some(s) = obj.get("language").and_then(|x| x.as_str()) {
            cfg.language = s.to_string();
        }
        if let Some(b) = obj.get("show_status_bar").and_then(|x| x.as_bool()) {
            cfg.show_status_bar = b;
        }
        if let Some(n) = obj.get("keep_alive_interval").and_then(|x| x.as_u64()) {
            cfg.keep_alive_interval = n;
        }
        if let Some(b) = obj.get("enable_key_macros").and_then(|x| x.as_bool()) {
            cfg.enable_key_macros = b;
        }
        // 旧 AI 字段 → 新结构
        if let Some(s) = obj.get("ai_api_key").and_then(|x| x.as_str()) {
            cfg.ai.api_key = s.to_string();
        }
        if let Some(s) = obj.get("ai_model").and_then(|x| x.as_str()) {
            cfg.ai.model = s.to_string();
        }
        if let Some(s) = obj.get("ai_api_base").and_then(|x| x.as_str()) {
            cfg.ai.base_url = s.to_string();
        }
        if let Some(s) = obj.get("ai_provider").and_then(|x| x.as_str()) {
            cfg.ai.provider = s.to_string();
        }
        Some(cfg)
    }

    /// 保存配置。主路径失败时回退到程序目录 `data/`；
    /// 两条路径都失败才返回错误（调用方应弹 toast 提示用户检查白名单）。
    pub fn save(&self) -> Result<(), String> {
        let data = serde_json::to_string_pretty(self)
            .map_err(|e| format!("序列化失败: {e}"))?;
        if let Some(path) = Self::config_path() {
            if Self::write_json(&path, &data).is_ok() {
                return Ok(());
            }
        }
        if let Some(path) = Self::fallback_path() {
            if Self::write_json(&path, &data).is_ok() {
                return Ok(());
            }
        }
        Err("主配置目录与程序目录均写入失败（可能被安全软件拦截），请把本程序加入杀毒软件白名单".to_string())
    }
}

// ---------------------------------------------------------------------------
// Keep-Alive 间隔（进程级全局量）
// ---------------------------------------------------------------------------

/// 当前生效的 Keep-Alive 间隔（秒；`0` = 关闭）。
///
/// 为什么不直接读 `AppConfig`：心跳任务跑在 tokio 里（`backends/ssh.rs`），
/// 那里没有 GPUI 的 `App`，也拿不到配置面板里的实体。而配置面板改完要求
/// **已经在跑的会话**也立即按新节奏打心跳，所以只能落在一个全局量上，
/// 由配置面板写、心跳任务读。
static KEEP_ALIVE_SECS: AtomicU64 = AtomicU64::new(30);

/// 心跳间隔（秒）。`0` 表示不发心跳。
pub fn keep_alive_interval() -> u64 {
    KEEP_ALIVE_SECS.load(Ordering::Relaxed)
}

/// 设置心跳间隔（配置加载 / 配置面板保存时调用）。
pub fn set_keep_alive_interval(secs: u64) {
    KEEP_ALIVE_SECS.store(secs, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// ZMODEM（rz/sz）开关与接收目录（同样是进程级全局量）
// ---------------------------------------------------------------------------

/// ZMODEM 自动收发总开关（默认开）。
static ZMODEM_ENABLED: AtomicBool = AtomicBool::new(true);

/// ZMODEM 接收目录的配置覆盖（None / 空串 = 走系统下载目录回退链）。
///
/// 为什么不用 `OnceLock`：配置面板保存后会反复写入新值，`Mutex<Option<_>>`
/// 的 `const new` 让它在 static 里直接可用，读侧也只多一次无竞争的锁。
static ZMODEM_DIR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// ZMODEM 自动收发是否开启。
pub fn zmodem_enabled() -> bool {
    ZMODEM_ENABLED.load(Ordering::Relaxed)
}

/// 配置里指定的 ZMODEM 接收目录（空白视为未配置）。
pub fn zmodem_download_dir() -> Option<PathBuf> {
    let g = ZMODEM_DIR.lock().ok()?;
    g.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// 设置 ZMODEM 开关与接收目录（配置加载 / 保存时调用）。
pub fn set_zmodem(enabled: bool, dir: Option<String>) {
    ZMODEM_ENABLED.store(enabled, Ordering::Relaxed);
    if let Ok(mut g) = ZMODEM_DIR.lock() {
        *g = dir;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 老配置（无 xinput_mode 字段）必须反序列化为默认 Xi2——不能因缺字段丢配置。
    #[test]
    fn missing_xinput_mode_field_defaults_to_xi2() {
        let cfg: AppConfig =
            serde_json::from_str(r#"{"font_size":14,"font_family":"m","theme":"dark","language":"zh-CN","show_status_bar":true,"keep_alive_interval":30,"enable_key_macros":true}"#)
                .expect("无新字段的老配置必须能解析");
        assert_eq!(cfg.xinput_mode, XInputMode::Xi2);
        assert!(xinput_enabled());
    }

    /// 字段名小写（lowercase）；Core 值往返保真，且缺省构造也是 Xi2。
    #[test]
    fn xinput_mode_round_trips() {
        assert_eq!(
            serde_json::to_string(&XInputMode::Xi2).unwrap(),
            r#""xi2""#
        );
        assert_eq!(
            serde_json::to_string(&XInputMode::Core).unwrap(),
            r#""core""#
        );
        let cfg: AppConfig = serde_json::from_str(
            r#"{"xinput_mode":"core","font_size":14,"font_family":"m","theme":"dark","language":"zh-CN","show_status_bar":true,"keep_alive_interval":30,"enable_key_macros":true}"#,
        )
        .expect("Core 模式配置必须能解析");
        assert_eq!(cfg.xinput_mode, XInputMode::Core);
        assert_eq!(AppConfig::default().xinput_mode, XInputMode::Xi2);
    }

    /// 进程级开关：set_xinput_mode(Core) 后 xinput_enabled()=false，恢复 Xi2 后为 true。
    /// （顺序敏感——同一进程内的其他测试可能与它并发，断言后恢复默认。）
    #[test]
    fn xinput_mode_global_gate_flips() {
        set_xinput_mode(XInputMode::Core);
        assert!(!xinput_enabled());
        set_xinput_mode(XInputMode::Xi2);
        assert!(xinput_enabled());
    }
}

// ---------------------------------------------------------------------------
// X server 输入协议开关（进程级全局量，与 ZMODEM 同款模式）
// ---------------------------------------------------------------------------

/// 内置 X server 的输入协议。
///
/// - `Xi2`（默认）：宣告 XInputExtension，GDK3 走 XI2 设备管理器 + 事件推送。
/// - `Core`：**不宣告** XInputExtension，客户端（GDK/firefox）自动退回核心
///   Motion/Button/Key 路径——XI2 万一再出问题时的一键排障开关。
///
/// 只影响**新连接**：扩展列表在连接握手时确定，已连接客户端不受影响。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum XInputMode {
    #[default]
    Xi2,
    Core,
}

/// XInputExtension 是否宣告（Xi2=true / Core=false）。默认 true。
static XINPUT_XI2_ENABLED: AtomicBool = AtomicBool::new(true);

/// 当前配置的输入协议。
pub fn xinput_mode() -> XInputMode {
    if XINPUT_XI2_ENABLED.load(Ordering::Relaxed) {
        XInputMode::Xi2
    } else {
        XInputMode::Core
    }
}

/// XInputExtension 是否宣告（Core 模式返回 false）。
pub fn xinput_enabled() -> bool {
    XINPUT_XI2_ENABLED.load(Ordering::Relaxed)
}

/// 设置输入协议（配置加载 / 设置面板保存时调用）。
pub fn set_xinput_mode(mode: XInputMode) {
    XINPUT_XI2_ENABLED.store(mode == XInputMode::Xi2, Ordering::Relaxed);
}

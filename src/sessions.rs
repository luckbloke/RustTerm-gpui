//! 会话数据模型与持久化（config/rustterm-gpui/sessions.json）。
//! 参照 XShell 风格：SSH 会话（可存密码）与 VNC/RDP/SPICE 远程桌面会话。

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionKind {
    Ssh,
    Vnc,
    Rdp,
    Spice,
}

impl SessionKind {
    pub fn label(&self) -> &'static str {
        match self {
            SessionKind::Ssh => "SSH",
            SessionKind::Vnc => "VNC",
            SessionKind::Rdp => "RDP",
            SessionKind::Spice => "SPICE",
        }
    }

    pub fn default_port(&self) -> u16 {
        match self {
            SessionKind::Ssh => 22,
            SessionKind::Vnc => 5900,
            SessionKind::Rdp => 3389,
            SessionKind::Spice => 5930,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub kind: SessionKind,
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub user: String,
    /// 密码（`auth_method == "password"` 时使用）
    #[serde(default)]
    pub password: String,
    /// 认证方式：`password`（默认）/ `key`
    #[serde(default)]
    pub auth_method: String,
    /// 私钥路径（`auth_method == "key"` 时使用）
    #[serde(default)]
    pub key_path: String,
    /// 域（RDP 登录用，可空）
    #[serde(default)]
    pub domain: String,
    /// RDP 共享给远端的本机目录（RDPDR 驱动器重定向；空 = 不共享）。
    ///
    /// 只有 RDP 有这个通道，VNC / SPICE 的会话里该字段恒为空。
    /// `default` 是必需的：加字段之前写下的 sessions.json 里没有这一项。
    #[serde(default)]
    pub share_dir: String,
    /// 旧版服务器兼容（RDP：禁用 NLA，仅用 TLS）。
    ///
    /// `default` 同样是为了兼容没有该字段的旧 `sessions.json`。
    #[serde(default)]
    pub legacy_compat: bool,
    /// SSH X11 转发：连接时发 `x11-req`，远端 X 程序显示在内置 X server。
    ///
    /// 只有 SSH 会话使用；`default` 兼容没有该字段的旧 `sessions.json`。
    #[serde(default)]
    pub x11: bool,
    #[serde(default = "default_group")]
    pub group: String,
}

impl SessionEntry {
    /// 是否使用密钥认证。
    pub fn uses_key(&self) -> bool {
        self.auth_method.eq_ignore_ascii_case("key")
    }
}

/// 会话分组缺省值（新建/反序列化共用，避免两处字面量走偏）。
pub fn default_group() -> String {
    "默认分组".to_string()
}

impl SessionEntry {
    pub fn new_id() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    /// 列表/最近会话里展示的名字。
    pub fn display(&self) -> String {
        if !self.name.is_empty() {
            return self.name.clone();
        }
        match self.kind {
            SessionKind::Ssh => {
                let cred = if self.uses_key() {
                    if self.key_path.is_empty() {
                        "未选密钥"
                    } else {
                        "密钥"
                    }
                } else if self.password.is_empty() {
                    "未存密码"
                } else {
                    "已存密码"
                };
                format!("{}@{}·{}", self.user, self.host, cred)
            }
            _ => format!("[{}] {}:{}", self.kind.label(), self.host, self.port),
        }
    }

    /// 搜索匹配（名称/主机/用户，忽略大小写）。
    pub fn matches_search(&self, query: &str) -> bool {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return true;
        }
        self.display().to_lowercase().contains(&q)
            || self.host.to_lowercase().contains(&q)
            || self.user.to_lowercase().contains(&q)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionStore {
    pub sessions: Vec<SessionEntry>,
    /// 最近会话（session id，最新在前）
    #[serde(default)]
    pub recent: Vec<String>,
}

impl SessionStore {
    pub fn path() -> Option<PathBuf> {
        dirs::config_dir().map(|p| p.join("rustterm-gpui").join("sessions.json"))
    }

    /// 旧版（egui）会话路径，仅用于首次运行时迁移读取。
    pub fn legacy_path() -> Option<PathBuf> {
        dirs::config_dir().map(|p| p.join("rustterm-egui").join("sessions.json"))
    }

    /// 回退路径：程序所在目录旁的 `data/sessions.json`。
    ///
    /// 主配置目录（%APPDATA%）可能被安全软件（360/管家等主动防御）拦截写入，
    /// 此时回退到程序目录，保证会话仍然能落盘。
    pub fn fallback_path() -> Option<PathBuf> {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("data").join("sessions.json")))
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
        // 主路径优先 → 旧版 egui 目录（一次性迁移）→ 回退路径
        if let Some(path) = Self::path() {
            if let Ok(data) = fs::read_to_string(&path) {
                if let Ok(store) = serde_json::from_str(&data) {
                    return store;
                }
            }
        }
        if let Some(path) = Self::legacy_path() {
            if let Ok(data) = fs::read_to_string(&path) {
                if let Ok(store) = serde_json::from_str(&data) {
                    return store;
                }
            }
        }
        if let Some(path) = Self::fallback_path() {
            if let Ok(data) = fs::read_to_string(&path) {
                if let Ok(store) = serde_json::from_str(&data) {
                    return store;
                }
            }
        }
        Self::seed()
    }

    /// 保存会话。主路径失败时自动回退到程序目录 `data/`，
    /// 两条路径都失败才返回错误（调用方弹 toast 提示用户）。
    pub fn save(&self) -> Result<(), String> {
        let data = serde_json::to_string_pretty(self)
            .map_err(|e| format!("序列化失败: {e}"))?;
        if let Some(path) = Self::path() {
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

    /// 首次运行的示例会话（与设计稿一致）。
    fn seed() -> Self {
        let mk = |kind: SessionKind, host: &str, port: u16, user: &str, password: &str| {
            SessionEntry {
                id: SessionEntry::new_id(),
                name: String::new(),
                kind,
                host: host.to_string(),
                port,
                user: user.to_string(),
                password: password.to_string(),
                auth_method: "password".to_string(),
                key_path: String::new(),
                domain: String::new(),
                share_dir: String::new(),
                legacy_compat: false,
                x11: false,
                group: default_group(),
            }
        };
        let ssh = mk(SessionKind::Ssh, "192.168.1.19", 22, "root", "********");
        let vnc1 = mk(SessionKind::Vnc, "192.168.1.16", 5900, "", "");
        let rdp = mk(SessionKind::Rdp, "192.168.1.16", 3389, "", "");
        let vnc2 = mk(SessionKind::Vnc, "192.168.1.16", 5902, "", "");
        let spice = mk(SessionKind::Spice, "192.168.1.19", 5930, "", "");
        Self {
            recent: vec![ssh.id.clone(), vnc1.id.clone(), rdp.id.clone(), vnc2.id.clone(), spice.id.clone()],
            sessions: vec![ssh, vnc1, rdp, vnc2, spice],
        }
    }

    pub fn by_id(&self, id: &str) -> Option<&SessionEntry> {
        self.sessions.iter().find(|s| s.id == id)
    }

    /// 记录一次连接（最近会话置顶，最多 8 条）。
    pub fn touch_recent(&mut self, id: &str) {
        self.recent.retain(|x| x != id);
        self.recent.insert(0, id.to_string());
        self.recent.truncate(8);
    }

    pub fn remove(&mut self, id: &str) {
        self.sessions.retain(|s| s.id != id);
        self.recent.retain(|x| x != id);
    }

    /// 覆盖更新一条会话（编辑用）。
    pub fn update(&mut self, entry: SessionEntry) {
        if let Some(slot) = self.sessions.iter_mut().find(|s| s.id == entry.id) {
            *slot = entry;
        } else {
            self.sessions.push(entry);
        }
    }
}

// ---------------------------------------------------------------------------
// 「恢复上次会话」用的标签快照（last_tabs.json）
// ---------------------------------------------------------------------------

/// 一个可恢复的标签。
///
/// **快照里不存任何密码**：SSH / 远程桌面只记 `session_id`（或主机+账户），
/// 恢复时回 `sessions.json` 取凭据。这样 `last_tabs.json` 单独泄露也不会多一份
/// 明文口令 —— 口令本来就只在用户显式「保存密码」时进 sessions.json。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LastTab {
    /// 本地终端
    Local,
    /// PTY 里跑的任意命令（Ping / traceroute 等）
    Command {
        program: String,
        #[serde(default)]
        args: Vec<String>,
    },
    /// Telnet（原始 TCP）
    Tcp { host: String, port: u16 },
    /// SSH
    Ssh {
        host: String,
        port: u16,
        user: String,
        /// 密钥路径（非机密，可以直接存）
        #[serde(default)]
        key_path: String,
        /// 来源会话（有则优先按它取凭据）
        #[serde(default)]
        session_id: Option<String>,
    },
    /// 内置浏览器
    Browser { url: String },
    /// 远程桌面
    Rd {
        /// `vnc` / `rdp` / `spice`（见 `RdTabProtocol::key`）
        protocol: String,
        host: String,
        port: u16,
        #[serde(default)]
        user: String,
        #[serde(default)]
        domain: String,
        #[serde(default)]
        share_dir: String,
        /// 旧版服务器兼容（禁用 NLA，仅用 TLS）
        #[serde(default)]
        legacy_compat: bool,
        #[serde(default)]
        session_id: Option<String>,
    },
    /// 本地文件浏览器
    Files,
}

/// 上次退出时的标签集合。
///
/// 只记「哪些标签开着」，不记分屏内部结构（那是 dock 自己的事）。
/// 读盘失败一律当空快照处理：这东西丢了最多是少个恢复入口，不该弹错误。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LastSession {
    #[serde(default)]
    pub tabs: Vec<LastTab>,
}

impl LastSession {
    /// 与 sessions.json 同目录（配置目录 / 回退目录都能落）。
    pub fn path() -> Option<PathBuf> {
        if let Some(p) = SessionStore::path() {
            if let Some(dir) = p.parent() {
                return Some(dir.join("last_tabs.json"));
            }
        }
        SessionStore::fallback_path()
            .and_then(|p| p.parent().map(|d| d.join("last_tabs.json")))
    }

    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            return Self::default();
        };
        let Ok(data) = fs::read_to_string(&path) else {
            return Self::default();
        };
        serde_json::from_str(&data).unwrap_or_default()
    }

    /// 尽力而为地写盘：失败静默（没有恢复入口比每次开标签弹错好）。
    pub fn save(&self) {
        let Some(path) = Self::path() else { return };
        let Ok(data) = serde_json::to_string_pretty(self) else {
            return;
        };
        let _ = SessionStore::write_json(&path, &data);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 老版本写下的 sessions.json 里没有 `share_dir`，
    /// 读的时候必须靠 `#[serde(default)]` 兜住，否则整个会话列表都解析失败
    /// （`load` 遇到反序列化错误会直接退回 `seed()`，等于用户会话全丢）。
    #[test]
    fn legacy_entry_without_share_dir_loads() {
        let json = r#"{
            "sessions": [{
                "id": "s1", "name": "", "kind": "Rdp", "host": "10.0.0.1", "port": 3389,
                "user": "u", "password": "p", "auth_method": "password",
                "key_path": "", "domain": "", "group": "默认分组"
            }],
            "recent": ["s1"]
        }"#;
        let store: SessionStore = serde_json::from_str(json).expect("老格式应能解析");
        assert_eq!(store.sessions.len(), 1);
        assert!(store.sessions[0].share_dir.is_empty());
    }

    #[test]
    fn share_dir_roundtrip() {
        let mut entry = SessionEntry {
            id: "s2".to_string(),
            name: String::new(),
            kind: SessionKind::Rdp,
            host: "10.0.0.2".to_string(),
            port: 3389,
            user: "u".to_string(),
            password: String::new(),
            auth_method: "password".to_string(),
            key_path: String::new(),
            domain: String::new(),
            share_dir: r"D:\share".to_string(),
            legacy_compat: false,
            x11: false,
            group: default_group(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: SessionEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.share_dir, r"D:\share");

        // 清空后也要能存成空串（不是缺失字段）
        entry.share_dir.clear();
        let back: SessionEntry = serde_json::from_str(&serde_json::to_string(&entry).unwrap()).unwrap();
        assert!(back.share_dir.is_empty());
    }

    /// 标签快照必须能往返，且**不能**把口令带进去。
    #[test]
    fn last_session_roundtrip_without_secrets() {
        let snap = LastSession {
            tabs: vec![
                LastTab::Local,
                LastTab::Tcp {
                    host: "10.0.0.3".to_string(),
                    port: 23,
                },
                LastTab::Ssh {
                    host: "10.0.0.4".to_string(),
                    port: 22,
                    user: "root".to_string(),
                    key_path: r"C:\keys\id_ed25519".to_string(),
                    session_id: Some("s1".to_string()),
                },
                LastTab::Rd {
                    protocol: "rdp".to_string(),
                    host: "10.0.0.5".to_string(),
                    port: 3389,
                    user: "u".to_string(),
                    domain: String::new(),
                    share_dir: r"D:\share".to_string(),
                    legacy_compat: true,
                    session_id: None,
                },
                LastTab::Browser {
                    url: "https://example.com".to_string(),
                },
                LastTab::Files,
            ],
        };
        let json = serde_json::to_string(&snap).unwrap();
        assert!(!json.contains("password"), "快照里不该出现口令字段: {json}");

        let back: LastSession = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tabs.len(), 6);
        match &back.tabs[3] {
            LastTab::Rd { protocol, share_dir, legacy_compat, .. } => {
                assert_eq!(protocol, "rdp");
                assert_eq!(share_dir, r"D:\share");
                assert!(legacy_compat);
            }
            other => panic!("第 4 项应是远程桌面，实际 {other:?}"),
        }
    }

    /// 未知/损坏的快照按空处理（老版本多一项也不该让恢复按钮炸掉）。
    #[test]
    fn broken_last_session_falls_back_to_empty() {
        assert!(serde_json::from_str::<LastSession>("not json")
            .map(|s| s.tabs)
            .unwrap_or_default()
            .is_empty());
    }
}

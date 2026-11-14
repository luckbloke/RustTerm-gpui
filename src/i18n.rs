//! 界面文案（中/英）。
//!
//! 只覆盖「界面骨架」：菜单、工具栏、侧栏、欢迎页、状态栏、快速连接栏。
//! 对话框正文与终端输出不翻译（终端输出本来就是远端内容）。
//!
//! 用 `tr(lang, key)` 而不是 HashMap：条目集中在一处，改文案时不用两头对齐。

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    Zh,
    En,
}

impl Lang {
    pub fn id(self) -> &'static str {
        match self {
            Lang::Zh => "zh-CN",
            Lang::En => "en-US",
        }
    }

    pub fn from_id(s: &str) -> Self {
        if s.starts_with("en") {
            Lang::En
        } else {
            Lang::Zh
        }
    }

    pub fn toggled(self) -> Self {
        match self {
            Lang::Zh => Lang::En,
            Lang::En => Lang::Zh,
        }
    }
}

#[derive(Clone, Copy)]
pub struct I18n {
    pub lang: Lang,
}

impl Default for I18n {
    fn default() -> Self {
        Self { lang: Lang::Zh }
    }
}

impl I18n {
    pub fn set_lang(&mut self, id: &str) {
        self.lang = Lang::from_id(id);
        self.sync_locale();
    }

    pub fn id(&self) -> &'static str {
        self.lang.id()
    }

    pub fn toggle(&mut self) {
        self.lang = self.lang.toggled();
        self.sync_locale();
    }

    /// 把当前语言同步给 gpui_kit 组件库（输入框右键菜单的剪切/复制/粘贴、
    /// 日历、按钮等内置文案走 rust_i18n）。
    ///
    /// ui.yml 的键：`zh-CN` 与 `en` 都有（main.rs 曾硬编码 "zh-CN"）。
    /// 这里集中同步，App::new 的 set_lang 与语言切换按钮都会走到。
    fn sync_locale(&self) {
        let locale = match self.lang {
            Lang::Zh => "zh-CN",
            Lang::En => "en",
        };
        gpui_kit::component::set_locale(locale);
    }

    #[allow(dead_code)]
    pub fn t(&self, key: &str) -> String {
        tr(self.lang, key)
    }
}

/// 取文案。未知 key 原样返回（便于发现漏配）。
pub fn tr(lang: Lang, key: &str) -> String {
    use Lang::{En, Zh};
    let s = match (lang, key) {
        // ---- 菜单栏 ----
        (Zh, "menu.terminal") => "终端",
        (En, "menu.terminal") => "Terminal",
        (Zh, "menu.session") => "会话",
        (En, "menu.session") => "Session",
        (Zh, "menu.view") => "视图",
        (En, "menu.view") => "View",
        (Zh, "menu.tools") => "工具",
        (En, "menu.tools") => "Tools",
        (Zh, "menu.settings") => "设置",
        (En, "menu.settings") => "Settings",
        (Zh, "menu.help") => "帮助",
        (En, "menu.help") => "Help",
        (Zh, "menu.x11") => "X 服务",
        (En, "menu.x11") => "X Service",
        (Zh, "x11.screen_name") => "X 屏幕",
        (En, "x11.screen_name") => "X screen",
        (Zh, "x11.screen_hint") => "X server 已随应用运行。SSH 会话启用 X11 转发后，远端 X 程序会显示在这里。",
        (En, "x11.screen_hint") => "The built-in X server is running. X11 programs from SSH sessions (with forwarding enabled) appear here.",
        (Zh, "x11.program_started") => "远端 X 程序已启动：工具 → X 服务 打开屏幕（已关闭自动显示）",
        (En, "x11.program_started") => "A remote X program started: Tools → X screen to open it (auto-show is off)",

        // ---- 终端菜单 ----
        (Zh, "item.local_connect") => "本地连接",
        (En, "item.local_connect") => "Local Connection",
        (Zh, "item.ssh_connect") => "SSH 连接...",
        (En, "item.ssh_connect") => "SSH Connection...",
        (Zh, "item.close_tab") => "关闭当前标签",
        (En, "item.close_tab") => "Close Tab",

        // ---- 分屏 ----
        (Zh, "item.split_right") => "向右分屏",
        (En, "item.split_right") => "Split Right",
        (Zh, "item.split_down") => "向下分屏",
        (En, "item.split_down") => "Split Down",
        (Zh, "item.move_into_split") => "把当前标签放进分屏",
        (En, "item.move_into_split") => "Move Tab into Split",
        (Zh, "item.close_pane") => "关闭当前窗格",
        (En, "item.close_pane") => "Close Pane",
        (Zh, "item.next_pane") => "下一个窗格",
        (En, "item.next_pane") => "Next Pane",
        // ---- 终端右键菜单 / 菜单栏「终端」项（含快捷键提示） ----
        (Zh, "item.copy") => "复制",
        (En, "item.copy") => "Copy",
        (Zh, "item.paste") => "粘贴",
        (En, "item.paste") => "Paste",
        (Zh, "item.select_all") => "全选",
        (En, "item.select_all") => "Select All",
        (Zh, "item.copy_sel") => "复制选区（Ctrl+Shift+C）",
        (En, "item.copy_sel") => "Copy Selection (Ctrl+Shift+C)",
        (Zh, "item.paste_clip") => "粘贴（Ctrl+V）",
        (En, "item.paste_clip") => "Paste (Ctrl+V)",
        (Zh, "item.find") => "查找（Ctrl+F）",
        (En, "item.find") => "Find (Ctrl+F)",
        (Zh, "ctx.split_right") => "向右分屏（Ctrl+Shift+D）",
        (En, "ctx.split_right") => "Split Right (Ctrl+Shift+D)",
        (Zh, "ctx.split_down") => "向下分屏（Ctrl+Shift+U）",
        (En, "ctx.split_down") => "Split Down (Ctrl+Shift+U)",
        (Zh, "ctx.next_pane") => "下一个窗格（Ctrl+Shift+P）",
        (En, "ctx.next_pane") => "Next Pane (Ctrl+Shift+P)",
        (Zh, "ctx.close_pane") => "关闭当前窗格（Ctrl+Shift+W）",
        (En, "ctx.close_pane") => "Close Pane (Ctrl+Shift+W)",
        // ---- 终端操作 toast ----
        (Zh, "toast.copied") => "已复制到剪贴板",
        (En, "toast.copied") => "Copied to clipboard",
        (Zh, "toast.no_selection") => "没有选中内容",
        (En, "toast.no_selection") => "Nothing selected",
        (Zh, "toast.pasted") => "已粘贴",
        (En, "toast.pasted") => "Pasted",
        (Zh, "toast.clipboard_empty") => "剪贴板没有文本",
        (En, "toast.clipboard_empty") => "Clipboard is empty",

        // ---- 会话菜单 ----
        (Zh, "item.new_session") => "新建会话...",
        (En, "item.new_session") => "New Session...",
        (Zh, "item.session_list") => "会话列表",
        (En, "item.session_list") => "Session List",
        (Zh, "item.refresh_sessions") => "刷新会话列表",
        (En, "item.refresh_sessions") => "Refresh Sessions",

        // ---- 视图菜单 ----
        (Zh, "item.browser") => "内置浏览器",
        (En, "item.browser") => "Built-in Browser",
        (Zh, "item.remote_desktop") => "远程桌面",
        (En, "item.remote_desktop") => "Remote Desktop",
        (Zh, "item.local_files") => "本地文件浏览器",
        (En, "item.local_files") => "Local Files",
        (Zh, "item.sftp_panel") => "SFTP 面板",
        (En, "item.sftp_panel") => "SFTP Panel",
        (Zh, "item.ai_assistant") => "AI 助手",
        (En, "item.ai_assistant") => "AI Assistant",
        (Zh, "item.status_bar") => "底部状态栏",
        (En, "item.status_bar") => "Status Bar",

        // ---- 工具 / 设置 / 帮助 ----
        (Zh, "item.ping") => "Ping",
        (En, "item.ping") => "Ping",
        (Zh, "item.telnet") => "Telnet",
        (En, "item.telnet") => "Telnet",
        (Zh, "item.portscan") => "端口扫描",
        (En, "item.portscan") => "Port Scan",
        (Zh, "item.macro_manager") => "键宏管理器",
        (En, "item.macro_manager") => "Macro Manager",
        (Zh, "item.config_panel") => "配置面板",
        (En, "item.config_panel") => "Settings Panel",
        (Zh, "item.ai_config") => "AI 配置",
        (En, "item.ai_config") => "AI Settings",
        (Zh, "item.dark_theme") => "深色主题",
        (En, "item.dark_theme") => "Dark Theme",
        (Zh, "item.light_theme") => "浅色主题",
        (En, "item.light_theme") => "Light Theme",
        (Zh, "item.about") => "关于 RustTerm",
        (En, "item.about") => "About RustTerm",
        (Zh, "item.x11_forward") => "启用 X11 转发",
        (En, "item.x11_forward") => "Enable X11 Forwarding",

        // ---- 工具栏 ----
        (Zh, "tb.local") => "本地会话",
        (En, "tb.local") => "Local",
        (Zh, "tb.ssh") => "SSH会话",
        (En, "tb.ssh") => "SSH",
        (Zh, "tb.sync") => "同步",
        (En, "tb.sync") => "Sync",
        (Zh, "tb.batch") => "批量",
        (En, "tb.batch") => "Batch",
        (Zh, "tb.tunnel") => "隧道",
        (En, "tb.tunnel") => "Tunnel",
        (Zh, "tb.find") => "查找",
        (En, "tb.find") => "Find",
        (Zh, "tb.files") => "本地文件",
        (En, "tb.files") => "Files",
        (Zh, "tb.browser") => "浏览器",
        (En, "tb.browser") => "Browser",
        (Zh, "tb.ai") => "AI",
        (En, "tb.ai") => "AI",
        (Zh, "tb.settings") => "设置",
        (En, "tb.settings") => "Settings",
        (Zh, "tb.language") => "语言",
        (En, "tb.language") => "Language",
        (Zh, "tb.theme") => "主题",
        (En, "tb.theme") => "Theme",
        (Zh, "tb.help") => "帮助",
        (En, "tb.help") => "Help",

        // ---- 侧栏 ----
        (Zh, "side.sessions") => "会话",
        (En, "side.sessions") => "Sessions",
        (Zh, "side.tools") => "工具",
        (En, "side.tools") => "Tools",
        (Zh, "side.macros") => "宏",
        (En, "side.macros") => "Macros",
        (Zh, "side.search") => "搜索会话…",
        (En, "side.search") => "Search sessions…",
        (Zh, "side.new") => "新建",
        (En, "side.new") => "New",

        // ---- 欢迎页 ----
        (Zh, "welcome.title") => "RustTerm 终端",
        (En, "welcome.title") => "RustTerm Terminal",
        (Zh, "welcome.subtitle") => "全功能终端客户端 · 本地 / SSH / 浏览器 / 远程桌面",
        (En, "welcome.subtitle") => "All-in-one terminal · Local / SSH / Browser / Remote Desktop",
        (Zh, "welcome.desc") => "选择一个标签页开始工作，或用下面的快捷键快速创建会话",
        (En, "welcome.desc") => "Pick a tab to get started, or use the shortcuts below",
        (Zh, "welcome.start_local") => "启动本地终端",
        (En, "welcome.start_local") => "Start Local Terminal",
        (Zh, "welcome.restore") => "恢复上次会话",
        (En, "welcome.restore") => "Restore Last Session",
        (Zh, "welcome.recent") => "最近会话",
        (En, "welcome.recent") => "Recent Sessions",
        (Zh, "welcome.search") => "搜索会话…（回车连接）",
        (En, "welcome.search") => "Search sessions… (Enter to connect)",

        // ---- 状态栏 ----
        (Zh, "status.opened") => "已打开",
        (En, "status.opened") => "Open",
        (Zh, "status.none") => "无",
        (En, "status.none") => "None",
        (Zh, "status.tabs") => "标签",
        (En, "status.tabs") => "Tabs",
        (Zh, "status.sessions") => "会话",
        (En, "status.sessions") => "Sessions",
        (Zh, "status.connected") => "已连接",
        (En, "status.connected") => "Connected",
        (Zh, "status.disconnected") => "已断开",
        (En, "status.disconnected") => "Disconnected",
        (Zh, "status.connecting") => "连接中...",
        (En, "status.connecting") => "Connecting...",
        (Zh, "status.browser") => "浏览器",
        (En, "status.browser") => "Browser",
        (Zh, "status.remote_active") => "远程桌面已连接",
        (En, "status.remote_active") => "Remote desktop connected",
        (Zh, "status.remote_connecting") => "远程桌面连接中",
        (En, "status.remote_connecting") => "Remote desktop connecting",
        (Zh, "status.remote_idle") => "远程桌面未连接",
        (En, "status.remote_idle") => "Remote desktop disconnected",
        (Zh, "status.failed") => "失败",
        (En, "status.failed") => "Failed",
        (Zh, "status.files") => "本地文件",
        (En, "status.files") => "Local Files",
        (Zh, "status.macros") => "键宏管理器",
        (En, "status.macros") => "Macro Manager",
        (Zh, "status.portscan") => "端口扫描",
        (En, "status.portscan") => "Port Scanner",
        (Zh, "status.portscan_running") => "端口扫描中...",
        (En, "status.portscan_running") => "Scanning...",

        // ---- 快速连接栏 ----
        (Zh, "qc.hint") => "快速连接...  例如: user@host:22",
        (En, "qc.hint") => "Quick connect...  e.g. user@host:22",
        (Zh, "qc.jump_hint") => "跳板机（可空）",
        (En, "qc.jump_hint") => "Jump host (optional)",
        (Zh, "qc.password") => "密码",
        (En, "qc.password") => "Password",
        (Zh, "qc.key") => "密钥",
        (En, "qc.key") => "Key",
        (Zh, "qc.connect") => "连接",
        (En, "qc.connect") => "Connect",
        (Zh, "qc.password_hint") => "密码（可空，连接后提示）",
        (En, "qc.password_hint") => "Password (optional)",

        // ---- 通用 ----
        (Zh, "common.language_switch") => "切换到 English",
        (En, "common.language_switch") => "Switch to 中文",

        // ---- AI 面板（ai_assistant.rs + app.rs 的 ai_context_menu_ui）----
        (Zh, "ai.title") => "AI 助手",
        (En, "ai.title") => "AI Assistant",
        (Zh, "ai.agent") => "Agent",
        (En, "ai.agent") => "Agent",
        (Zh, "ai.tooltip_screenshot") => "截取屏幕并附加",
        (En, "ai.tooltip_screenshot") => "Capture screen and attach",
        (Zh, "ai.tooltip_clear") => "清空对话",
        (En, "ai.tooltip_clear") => "Clear conversation",
        (Zh, "ai.tooltip_settings") => "AI 设置",
        (En, "ai.tooltip_settings") => "AI Settings",
        (Zh, "ai.tooltip_close") => "关闭 AI 面板",
        (En, "ai.tooltip_close") => "Close AI panel",
        (Zh, "ai.read_only") => "🔒 只读",
        (En, "ai.read_only") => "🔒 Read-only",
        (Zh, "ai.welcome_title") => "你好，我是 RustTerm AI 助手",
        (En, "ai.welcome_title") => "Hi, I'm the RustTerm AI assistant",
        (Zh, "ai.welcome_sub") => "可以询问运维命令、排查问题、分析日志等",
        (En, "ai.welcome_sub") => "Ask about ops commands, troubleshooting, log analysis, and more",
        (Zh, "ai.no_api_key") => "ℹ 当前未填 API Key，将按无鉴权方式请求（本地网关可用）",
        (En, "ai.no_api_key") => "ℹ No API Key set — requests go out unauthenticated (local gateways work)",
        (Zh, "ai.placeholder") => "问点什么...（Enter 发送）",
        (En, "ai.placeholder") => "Ask something... (Enter to send)",
        (Zh, "ai.copy_btn") => "📋 复制",
        (En, "ai.copy_btn") => "📋 Copy",
        (Zh, "ai.toast_copied") => "已复制消息内容",
        (En, "ai.toast_copied") => "Message copied",
        (Zh, "ai.task_done") => "✅ 任务已完成",
        (En, "ai.task_done") => "✅ Task completed",
        (Zh, "ai.readonly_blocked") => "🔒 只读模式已禁用",
        (En, "ai.readonly_blocked") => "🔒 Disabled in read-only mode",
        (Zh, "ai.dangerous") => "⚠ 危险",
        (En, "ai.dangerous") => "⚠ Dangerous",
        (Zh, "ai.send_to_terminal") => "发送到终端",
        (En, "ai.send_to_terminal") => "Send to terminal",
        (Zh, "ai.dangerous_disabled") => "⚠ 危险，已禁用",
        (En, "ai.dangerous_disabled") => "⚠ Dangerous, disabled",
        (Zh, "ai.thinking") => "思考中…",
        (En, "ai.thinking") => "Thinking…",
        (Zh, "ai.stop") => "停止",
        (En, "ai.stop") => "Stop",
        (Zh, "ai.attach_hint") => "📎 发送时自动附带：{hint} + 最近 {n} 行终端输出",
        (En, "ai.attach_hint") => "📎 Attaches on send: {hint} + last {n} lines of terminal output",
        (Zh, "ai.file_chip") => "📄 {name}（{size}）点击移除",
        (En, "ai.file_chip") => "📄 {name} ({size}) — click to remove",
        (Zh, "ai.shot_chip") => "🖼 {name}（{w}×{h}）点击移除",
        (En, "ai.shot_chip") => "🖼 {name} ({w}×{h}) — click to remove",
        (Zh, "ai.attach") => "附加",
        (En, "ai.attach") => "Attach",
        (Zh, "ai.send") => "发送",
        (En, "ai.send") => "Send",
        (Zh, "ai.clear_input") => "清空输入",
        (En, "ai.clear_input") => "Clear input",
        (Zh, "ai.history_count") => "历史 {n} 条",
        (En, "ai.history_count") => "{n} messages in history",
        (Zh, "ai.menu_copy_sel") => "复制选中",
        (En, "ai.menu_copy_sel") => "Copy selected",
        (Zh, "ai.menu_copy_whole") => "复制整条消息",
        (En, "ai.menu_copy_whole") => "Copy whole message",
        (Zh, "ai.menu_copy_all") => "复制全部对话",
        (En, "ai.menu_copy_all") => "Copy whole conversation",
        (Zh, "ai.toast_sel_copied") => "已复制选中文字",
        (En, "ai.toast_sel_copied") => "Selected text copied",
        (Zh, "ai.toast_no_sel_copy") => "未选中文字，已复制整条消息",
        (En, "ai.toast_no_sel_copy") => "No selection — copied the whole message",
        (Zh, "ai.toast_whole_copied") => "已复制整条消息",
        (En, "ai.toast_whole_copied") => "Whole message copied",
        (Zh, "ai.toast_all_copied") => "已复制全部对话",
        (En, "ai.toast_all_copied") => "Whole conversation copied",
        (Zh, "ai.err_cancelled") => "已取消本次请求。",
        (En, "ai.err_cancelled") => "Request cancelled.",
        (Zh, "ai.err_no_base_url") => "未配置 Base URL，请点击右上角 ⚙ 打开 AI 配置。",
        (En, "ai.err_no_base_url") => "Base URL not set — click ⚙ in the top-right to open AI settings.",
        (Zh, "ai.err_no_model") => "尚未配置模型名，请点击右上角 ⚙ 打开 AI 配置。",
        (En, "ai.err_no_model") => "Model name not set — click ⚙ in the top-right to open AI settings.",
        (Zh, "ai.hist_truncated") => "\n（历史消息过长，已截断）",
        (En, "ai.hist_truncated") => "\n(history too long, truncated)",
        (Zh, "ai.config_error_hint") => "\n\n（点击右上角 ⚙ 打开 AI 配置）",
        (En, "ai.config_error_hint") => "\n\n(Click ⚙ in the top-right to open AI settings)",
        (Zh, "ai.err_worker_died") => "AI 请求线程异常退出（没有返回结果），已恢复可发送状态。",
        (En, "ai.err_worker_died") => "AI request thread died without a result — sending re-enabled.",
        (Zh, "ai.err_timeout") => "等待 AI 回复超时（{n} 秒），已放弃本次请求。",
        (En, "ai.err_timeout") => "Timed out waiting for the AI reply ({n}s) — request dropped.",
        (Zh, "ai.err_too_many_attachments") => "附件太多（最多 {n} 个：截图 + 文件），先移除一些再试。",
        (En, "ai.err_too_many_attachments") => "Too many attachments (max {n}: screenshots + files) — remove some first.",
        (Zh, "ai.err_pick_thread") => "文件选择线程异常退出。",
        (En, "ai.err_pick_thread") => "File picker thread died unexpectedly.",
        (Zh, "ai.err_shot_max") => "最多附加 {n} 张截图，先移除一张再截。",
        (En, "ai.err_shot_max") => "At most {n} screenshots — remove one before capturing.",
        (Zh, "ai.err_shot_encode") => "截图编码失败: {e}",
        (En, "ai.err_shot_encode") => "Screenshot encode failed: {e}",
        (Zh, "ai.err_shot_capture") => "截图失败: {e}",
        (En, "ai.err_shot_capture") => "Screenshot failed: {e}",
        (Zh, "ai.pick_dialog_title") => "选择要附加的文件",
        (En, "ai.pick_dialog_title") => "Select files to attach",
        (Zh, "ai.file_size_chars") => "{n} 字",
        (En, "ai.file_size_chars") => "{n} chars",
        (Zh, "ai.file_size_img") => "图片 {n} KB",
        (En, "ai.file_size_img") => "Image {n} KB",
        (Zh, "ai.agent_confirm_title") => "⚠ 危险命令，请确认",
        (En, "ai.agent_confirm_title") => "⚠ Dangerous command — confirm",
        (Zh, "ai.agent_confirm_run") => "执行",
        (En, "ai.agent_confirm_run") => "Run",
        (Zh, "ai.agent_confirm_skip") => "跳过",
        (En, "ai.agent_confirm_skip") => "Skip",
        (Zh, "ai.agent_confirm_abort") => "终止本轮",
        (En, "ai.agent_confirm_abort") => "Abort run",
        (Zh, "ai.agent_stop") => "停止 Agent",
        (En, "ai.agent_stop") => "Stop Agent",

        (_, other) => return other.to_string(),
    };
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_key_translates() {
        assert_eq!(tr(Lang::Zh, "menu.terminal"), "终端");
        assert_eq!(tr(Lang::En, "menu.terminal"), "Terminal");
    }

    #[test]
    fn unknown_key_falls_back_to_key() {
        assert_eq!(tr(Lang::En, "no.such.key"), "no.such.key");
    }

    #[test]
    fn lang_id_roundtrip() {
        assert_eq!(Lang::from_id("en-US"), Lang::En);
        assert_eq!(Lang::from_id("zh-CN"), Lang::Zh);
        assert_eq!(Lang::Zh.toggled(), Lang::En);
    }

    /// 界面里每个 `tr(lang, "键")` 用到的键都必须在**两种语言**里都有译文。
    ///
    /// 漏译不会报错：`tr` 会原样返回键名，界面上就直接显示
    /// `item.move_into_split` 这种字符串。新增菜单项/文案时最容易漏这一步
    /// （加「把当前标签放进分屏」时就是这么漏掉的），所以用测试兜住。
    #[test]
    fn every_referenced_key_has_both_translations() {
        let sources = [
            include_str!("app.rs"),
            include_str!("rdp.rs"),
            include_str!("terminal_view.rs"),
            include_str!("sftp.rs"),
            include_str!("filebrowser.rs"),
            include_str!("ai_assistant.rs"),
        ];
        let marker = "tr(lang, \"";
        let markers = ["tr(lang, \"", "tr(self.lang, \""];
        let mut checked = 0usize;
        let mut missing: Vec<&str> = Vec::new();
        for src in sources {
            for marker in markers {
                let mut rest = src;
                while let Some(pos) = rest.find(marker) {
                    rest = &rest[pos + marker.len()..];
                    let Some(end) = rest.find('"') else { break };
                    let key = &rest[..end];
                    rest = &rest[end..];
                    if key.is_empty() || key.contains('{') || key.contains(' ') {
                        continue; // 不是静态键（格式化或表达式），跳过
                    }
                    checked += 1;
                    if tr(Lang::Zh, key) == key || tr(Lang::En, key) == key {
                        missing.push(key);
                    }
                }
            }
        }
        assert!(checked > 50, "扫描到的键太少（{checked}），扫描逻辑可能已失效");
        missing.sort_unstable();
        missing.dedup();
        assert!(missing.is_empty(), "缺少译文的键：{missing:?}");
    }
}

//! AI 助手后端：多提供方（OpenAI 兼容 / Ollama / Anthropic）对话调用。
//!
//! - 请求在后台线程阻塞执行（ureq），结果经 mpsc 通道回传 UI。
//! - 统一消息结构，各提供方在 `build_request` / `parse_response` 中做协议差异适配。
//! - 附带终端上下文（最近 N 行）与只读模式下的写命令拦截。

use crate::config::{AiProvider, AiSettings};
use serde_json::{json, Value};

/// 一条对话消息。
#[derive(Clone, Debug)]
pub struct AiMessage {
    /// `system` / `user` / `assistant`
    pub role: String,
    pub content: String,
    /// 附带的图片（PNG 的 base64，不含 data: 前缀）
    pub images: Vec<String>,
}

impl AiMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self { role: "system".into(), content: content.into(), images: Vec::new() }
    }
    #[allow(dead_code)]
    pub fn user(content: impl Into<String>) -> Self {
        Self { role: "user".into(), content: content.into(), images: Vec::new() }
    }
    #[allow(dead_code)]
    pub fn assistant(content: impl Into<String>) -> Self {
        Self { role: "assistant".into(), content: content.into(), images: Vec::new() }
    }
    /// 带截图的用户消息。
    pub fn user_with_images(content: impl Into<String>, images: Vec<String>) -> Self {
        Self { role: "user".into(), content: content.into(), images }
    }
}

/// 系统提示词：运维助手风格，命令放 bash 代码块。
/// Agent 模式段照抄主工程 rustterm-tauri ai.rs L63-79（真实使用验证过）。
pub fn system_prompt(agent_mode: bool) -> String {
    let base = "你是 RustTerm 的终端运维助手。用户会在终端里操作服务器，你需要：\n\
         1. 用简洁的中文回答用户的问题。\n\
         2. 如果需要执行命令，把命令放在 ```bash 代码块里。\n\
         3. 命令要能在 Linux/macOS 的 bash 或 Windows 的 PowerShell 里运行。\n\
         4. 不要执行危险命令（rm -rf /、format、dd 等），如果用户要求，先警告。\n\
         5. 一次最多给 3 条命令，按顺序执行。\n";
    if agent_mode {
        base.to_string()
            + "\n【Agent 模式】\n\
               - 每次回复只给 1-2 条命令，等用户反馈输出后再决定下一步。\n\
               - 命令放在 ```bash 代码块里。\n\
               - 命令执行后，用户会把输出作为下一条消息发给你。\n\
               - 当你认为任务已经完成，在回复末尾加上 [DONE]。\n\
               - 如果命令失败或输出异常，可以调整策略重试，但不要重复同一条命令超过 2 次。\n\
               - 最多 10 轮，超过就停止。\n"
    } else {
        base.to_string()
    }
}

/// 错误类型：便于 UI 区分「配置问题」与「网络问题」。
#[derive(Clone, Debug)]
pub struct AiError {
    pub message: String,
    /// 是否属于配置类错误（缺 Key / 缺 Base URL）
    pub config_error: bool,
}

impl AiError {
    fn config(msg: impl Into<String>) -> Self {
        Self { message: msg.into(), config_error: true }
    }
    fn other(msg: impl Into<String>) -> Self {
        Self { message: msg.into(), config_error: false }
    }
}

impl std::fmt::Display for AiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// 发起一次对话请求（阻塞，运行在后台线程）。
pub fn chat(cfg: &AiSettings, agent_mode: bool, history: &[AiMessage]) -> Result<String, AiError> {
    let provider = cfg.provider();
    let base = cfg.trimmed_base();
    if base.is_empty() {
        return Err(AiError::config("未配置 Base URL，请点击 AI 助手的 ⚙ 设置"));
    }
    // API Key 允许留空：本地网关（one-api / LM Studio / vLLM）常见无鉴权部署，
    // 是否带 Authorization 头由 build_request 按 key 是否为空决定。
    if cfg.model.trim().is_empty() {
        return Err(AiError::config("未配置模型名"));
    }

    let mut messages: Vec<AiMessage> = vec![AiMessage::system(system_prompt(agent_mode))];
    messages.extend_from_slice(history);

    let (url, headers, body) = build_request(provider, &base, cfg, &messages);

    let mut req = ureq::post(&url).timeout(std::time::Duration::from_secs(120));
    for (k, v) in &headers {
        req = req.set(k, v);
    }
    let resp = req.send_json(body).map_err(|e| match e {
        ureq::Error::Status(code, resp) => {
            let text = resp.into_string().unwrap_or_default();
            AiError::other(format!("HTTP {code}: {}", truncate(&text, 400)))
        }
        ureq::Error::Transport(t) => AiError::other(format!("网络请求失败: {t}")),
    })?;

    let v: Value = resp
        .into_json()
        .map_err(|e| AiError::other(format!("解析响应失败: {e}")))?;

    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| err.to_string());
        return Err(AiError::other(format!("API 错误: {msg}")));
    }

    parse_response(provider, &v)
}

/// OpenAI 兼容协议的消息体：带图时 content 变成数组（vision 模型要求）。
fn openai_message(m: &AiMessage) -> Value {
    if m.images.is_empty() {
        return json!({"role": m.role, "content": m.content});
    }
    let mut parts = vec![json!({"type": "text", "text": m.content})];
    for img in &m.images {
        parts.push(json!({
            "type": "image_url",
            "image_url": {"url": format!("data:image/png;base64,{img}")}
        }));
    }
    json!({"role": m.role, "content": parts})
}

/// 构造 URL / 头部 / 请求体。
fn build_request(
    provider: AiProvider,
    base: &str,
    cfg: &AiSettings,
    messages: &[AiMessage],
) -> (String, Vec<(String, String)>, Value) {
    match provider {
        AiProvider::Openai => {
            let url = format!("{base}/chat/completions");
            // 空 Key 时不发 Authorization：兼容本地无鉴权网关
            let key = cfg.api_key.trim();
            let mut headers: Vec<(String, String)> = Vec::new();
            if !key.is_empty() {
                headers.push(("Authorization".to_string(), format!("Bearer {key}")));
            }
            let msgs: Vec<Value> = messages.iter().map(openai_message).collect();
            let body = json!({
                "model": cfg.model,
                "messages": msgs,
                "max_tokens": cfg.max_tokens,
                "temperature": cfg.temperature,
            });
            (url, headers, body)
        }
        AiProvider::Ollama => {
            // Ollama 原生接口；用户若填了 /v1 兼容端点也能命中 /chat/completions。
            let url = if base.ends_with("/v1") {
                format!("{base}/chat/completions")
            } else {
                format!("{base}/api/chat")
            };
            let headers: Vec<(String, String)> = Vec::new();
            let msgs: Vec<Value> = messages.iter().map(openai_message).collect();
            let body = json!({
                "model": cfg.model,
                "messages": msgs,
                "stream": false,
                "options": {
                    "temperature": cfg.temperature,
                    "num_predict": cfg.max_tokens,
                },
            });
            (url, headers, body)
        }
        AiProvider::Anthropic => {
            let url = format!("{base}/v1/messages");
            let key = cfg.api_key.trim();
            let mut headers: Vec<(String, String)> = vec![
                ("anthropic-version".to_string(), "2023-06-01".to_string()),
            ];
            if !key.is_empty() {
                headers.push(("x-api-key".to_string(), key.to_string()));
            }
            // Anthropic 用顶层 system 字段承载系统提示词。
            let mut system = String::new();
            let mut msgs: Vec<Value> = Vec::new();
            for m in messages {
                if m.role == "system" {
                    system.push_str(&m.content);
                    system.push('\n');
                } else if m.images.is_empty() {
                    msgs.push(json!({"role": m.role, "content": m.content}));
                } else {
                    let mut parts = vec![json!({"type": "text", "text": m.content})];
                    for img in &m.images {
                        parts.push(json!({
                            "type": "image",
                            "source": {"type": "base64", "media_type": "image/png", "data": img}
                        }));
                    }
                    msgs.push(json!({"role": m.role, "content": parts}));
                }
            }
            let body = json!({
                "model": cfg.model,
                "system": system.trim(),
                "messages": msgs,
                "max_tokens": cfg.max_tokens,
                "temperature": cfg.temperature,
            });
            (url, headers, body)
        }
    }
}

/// 解析各提供方的响应体。
fn parse_response(provider: AiProvider, v: &Value) -> Result<String, AiError> {
    let content = match provider {
        AiProvider::Openai => v
            .pointer("/choices/0/message/content")
            .and_then(|c| c.as_str())
            .map(|s| s.to_string()),
        AiProvider::Ollama => v
            .pointer("/message/content")
            .and_then(|c| c.as_str())
            .map(|s| s.to_string()),
        AiProvider::Anthropic => v
            .pointer("/content/0/text")
            .and_then(|c| c.as_str())
            .map(|s| s.to_string()),
    };
    match content {
        Some(text) if !text.trim().is_empty() => Ok(text),
        Some(_) => Ok("(空响应)".to_string()),
        None => Err(AiError::other(format!(
            "响应格式异常: {}",
            truncate(&v.to_string(), 300)
        ))),
    }
}

/// 从 AI 回复里提取 ```bash 代码块中的命令。
pub fn extract_commands(reply: &str) -> Vec<String> {
    let mut commands = Vec::new();
    let mut in_block = false;
    let mut current = String::new();
    for line in reply.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            if in_block {
                if !current.trim().is_empty() {
                    commands.push(current.trim().to_string());
                }
                current.clear();
                in_block = false;
            } else {
                in_block = true;
            }
            continue;
        }
        if in_block {
            current.push_str(line);
            current.push('\n');
        }
    }
    commands
}

/// 危险命令检测。
pub fn is_dangerous(cmd: &str) -> bool {
    let lower = cmd.to_lowercase();
    [
        "rm -rf /", "rm -rf /*", "mkfs", "dd if=", "format ",
        ":(){ :|:& };:", "> /dev/sda", "chmod -r 777 /",
        "shutdown", "reboot", "init 0", "halt",
    ]
    .iter()
    .any(|p| lower.contains(p))
}

/// 判断命令是否会修改系统状态（只读模式下禁止自动执行）。
pub fn is_write_command(cmd: &str) -> bool {
    let lower = cmd.to_lowercase();
    [
        ">", ">>", "rm ", "mv ", "cp ", "mkdir ", "touch ",
        "chmod ", "chown ", "chgrp ",
        "apt ", "yum ", "dnf ", "pacman ", "brew ",
        "systemctl ", "service ", "kill ", "pkill ",
        "useradd", "usermod", "userdel", "passwd",
        "dd ", "mkfs", "fdisk", "parted",
        "sed -i", "tee ",
        "git clone", "pip install", "npm install",
    ]
    .iter()
    .any(|p| lower.contains(p))
}

/// 判断 AI 回复是否表示任务完成。
pub fn is_task_done(reply: &str) -> bool {
    reply.contains("[DONE]")
}

// ---------------------------------------------------------------------------
// Agent 回复解析：**双格式兼容**（JSON 优先 + bash 块/[DONE] 降级）
// ---------------------------------------------------------------------------

/// 一轮 Agent 回复的结构化结果（两种格式解析后的统一形态）。
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct AgentPlan {
    /// 模型的简短推理/说明（JSON 的 `think`；bash 协议 = 代码块外正文）。
    pub think: String,
    /// 本轮要执行的命令（按序；每条仍会过 is_dangerous 安全闸）。
    pub commands: Vec<String>,
    /// 任务是否完成。
    pub done: bool,
    /// 完成时的总结（done=true 时有值）。
    pub summary: Option<String>,
}

/// 剥 ```json 围栏：返回围栏内的 JSON 文本；整条 trim 后以 { 开头也算。
/// 没有 JSON 候选 → None。
fn json_candidate(reply: &str) -> Option<&str> {
    let trimmed = reply.trim();
    // 容忍大小写与语言标记变体：找 "```json"（大小写不敏感）
    let lower = trimmed.to_lowercase();
    if let Some(pos) = lower.find("```json") {
        let rest = &trimmed[pos + 7..];
        // 闭合取 opener 之后的**第一个** ``` 行（rfind 会跨块吞掉 bash 块）
        let end = rest.find("```").unwrap_or(rest.len());
        return Some(rest[..end].trim());
    }
    if trimmed.starts_with('{') {
        return Some(trimmed);
    }
    None
}

/// 行级移除 ```json ... ``` 块（opener 到第一个闭合 ``` 行，含两端）。
/// 降级路径的 bash 提取用它，避免坏 JSON 块的残余被当成命令。
fn remove_json_blocks(reply: &str) -> String {
    let mut out = String::new();
    let mut skipping = false;
    for line in reply.lines() {
        let t = line.trim();
        if !skipping && t.to_lowercase().starts_with("```json") {
            skipping = true;
            continue;
        }
        if skipping {
            // 块内：遇到闭合围栏行就结束跳过
            if t.starts_with("```") {
                skipping = false;
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// bash 协议的 think：代码块（``` 围栏）之外的正文，去掉 [DONE] 标记。
fn prose_outside_blocks(reply: &str) -> String {
    let mut out = String::new();
    let mut in_block = false;
    for line in reply.lines() {
        let t = line.trim();
        if t.starts_with("```") {
            in_block = !in_block;
            continue;
        }
        if !in_block {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.replace("[DONE]", "").trim().to_string()
}

/// 解析 Agent 模型回复（双格式兼容）：
/// 1. **JSON 优先**：剥 ```json 围栏与前后杂文 → `{think, commands, done,
///    summary}`，字段缺失取默认值（think=""、commands=[]、done=false）；
/// 2. **失败降级**：bash 块 + [DONE]（tauri 约定），think = 块外正文；
/// 3. 两者都无内容 → 空 plan（commands 空 + done=false）→ 状态机按现有
///    降级语义终止本轮（普通文本展示）。
///
/// 注意：JSON 解析成功但不含协议字段（如模型恰好回了别的 JSON 对象）也
/// 会得到空 commands + done=false → 同样走降级终止，不会误执行。
pub fn parse_agent_reply(reply: &str) -> AgentPlan {
    if let Some(json_text) = json_candidate(reply) {
        if let Ok(v) = serde_json::from_str::<Value>(json_text) {
            if v.is_object() {
                let think = v
                    .get("think")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let commands = v
                    .get("commands")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                let done = v.get("done").and_then(Value::as_bool).unwrap_or(false);
                let summary = v
                    .get("summary")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                return AgentPlan { think, commands, done, summary };
            }
        }
    }
    // 降级：先把 ```json 块行级移除（坏 JSON 块的残余不能被 bash 扫块
    // 当成命令），再做 bash 块 + [DONE] 提取。
    let sans = remove_json_blocks(reply);
    AgentPlan {
        think: prose_outside_blocks(&sans),
        commands: extract_commands(&sans),
        done: is_task_done(&sans),
        summary: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 强模型 JSON fixture（含 ```json 围栏 + 前后杂文）。
    const JSON_FIXTURE: &str = "让我先看看目录结构。\n```json\n{\"think\": \"先列目录再排序\", \"commands\": [\"du -a .\", \"sort -n\"], \"done\": false}\n```\n以上请执行。";

    /// 弱模型 bash fixture（降级路径：无 JSON，bash 块 + [DONE]）。
    const BASH_FIXTURE: &str = "好的，依次执行：\n```bash\nls -la\n```\n完成后汇总。[DONE]";

    #[test]
    fn json_fixture_parses_structured() {
        let plan = parse_agent_reply(JSON_FIXTURE);
        assert_eq!(plan.think, "先列目录再排序");
        assert_eq!(plan.commands, ["du -a .", "sort -n"]);
        assert!(!plan.done);
        assert_eq!(plan.summary, None);
    }

    #[test]
    fn json_done_fixture_carries_summary() {
        let plan = parse_agent_reply(
            "```json\n{\"think\": \"结束\", \"done\": true, \"summary\": \"最大文件是 big.tar\"}\n```",
        );
        assert!(plan.done);
        assert_eq!(plan.summary.as_deref(), Some("最大文件是 big.tar"));
        assert!(plan.commands.is_empty());
    }

    #[test]
    fn json_without_fence_or_prose_is_parsed() {
        let plan = parse_agent_reply(
            r#"{"think": "t", "commands": ["ls"], "done": false}"#,
        );
        assert_eq!(plan.commands, ["ls"]);
    }

    #[test]
    fn bash_fixture_falls_back_and_extracts() {
        let plan = parse_agent_reply(BASH_FIXTURE);
        // 降级：bash 块命令 + [DONE] 判完成；think = 块外正文
        assert_eq!(plan.commands, ["ls -la"]);
        assert!(plan.done);
        assert!(plan.think.contains("好的，依次执行："));
        assert!(!plan.think.contains("[DONE]"));
    }

    /// 坏 JSON（缺引号/被截断）→ 降级 bash 协议；都无内容 → 空 plan。
    #[test]
    fn broken_json_falls_back_or_degrades() {
        // 坏 JSON + 有 bash 块 → 降级提取成功
        let plan = parse_agent_reply("```json\n{broken\n```\n```bash\necho hi\n```");
        assert_eq!(plan.commands, ["echo hi"]);
        // 坏 JSON 且无 bash 块 → 空 plan（状态机按普通文本展示并终止）
        let plan = parse_agent_reply("```json\n{broken\n```");
        assert!(plan.commands.is_empty());
        assert!(!plan.done);
    }

    /// 安全闸一致性：JSON 与 bash 两种格式的 commands 走同一条
    /// is_dangerous 闸（状态机统一派发前检查，这里验证解析产物本身）。
    #[test]
    fn safety_gate_sees_commands_from_both_formats() {
        let json_plan = parse_agent_reply(
            r#"```json
{"think": "清理", "commands": ["rm -rf /tmp/x"], "done": false}
```"#,
        );
        let bash_plan = parse_agent_reply("```bash\nrm -rf /tmp/x\n```");
        for plan in [json_plan, bash_plan] {
            assert_eq!(plan.commands.len(), 1);
            assert!(is_dangerous(&plan.commands[0]), "两格式的命令都要过安全闸");
        }
    }

    /// 杂文容忍：JSON 前后大段散文不影响解析。
    #[test]
    fn json_with_surrounding_prose_is_tolerated() {
        let plan = parse_agent_reply(
            "计划如下，请查收。\n第一行说明。\n```json\n{\"think\":\"go\",\"commands\":[\"ls\"],\"done\":false}\n```\n第二行说明。",
        );
        assert_eq!(plan.commands, ["ls"]);
        assert_eq!(plan.think, "go");
    }
}

/// 截断过长文本（保留头部）。
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}…")
}

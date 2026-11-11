//! 真 Agent 终端执行循环：**纯函数状态机**（输入事件 → 输出动作）。
//!
//! ## 协议（对齐主工程 rustterm-tauri 的成熟约定，不是 JSON）
//! - 模型每轮把命令放 ```bash 代码块（解析用 `ai::extract_commands` 扫块）；
//! - 任务完成 = 回复含 **[DONE]**（`ai::is_task_done`）；
//! - 上限对齐 tauri：**10 轮**模型回合、单轮 ≤3 条命令、同一条命令不重复超 2 次。
//!
//! ## 设计
//! `AgentLoop::next(event) -> Vec<AgentAction>`：状态机的输入输出全是数据，
//! 模型回复用固定 fixture 模拟即可离线测试（不需要真 AI）。安全边界全部在
//! 状态机里：危险命令暂停三选一、只读由入口闸、步数上限、同命令去重、
//! 停止立即终止、终端目标死亡即中止。
//!
//! `Say` 动作的文案目前为中文（agent.rs 无 lang 上下文；面板其余 UI 已 i18n）。

use std::collections::{HashMap, VecDeque};

/// 模型回合上限（对齐 tauri 版）。
pub const MAX_STEPS: usize = 10;
/// 单个模型回合最多执行的命令数（对齐 tauri 版「一次最多 3 条」）。
pub const MAX_COMMANDS_PER_TURN: usize = 3;
/// 同一条命令最多执行次数（超过就丢弃，防死循环）。
pub const MAX_SAME_COMMAND_RUNS: usize = 2;

/// 喂给模型的历史条目（role: "user" / "assistant"）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryEntry {
    pub role: String,
    pub content: String,
}

/// 状态机输入事件。
#[derive(Clone, Debug)]
pub enum AgentEvent {
    /// 用户下达任务（snapshot = 当前终端输出快照）。
    Start { task: String, snapshot: String },
    /// 模型回复（plan 由 `ai::parse_agent_reply` 双格式解析产出：
    /// JSON 优先，失败降级 bash 块 + [DONE]）。
    ModelReply { plan: crate::ai::AgentPlan },
    /// 一条命令已在终端执行完（output = 捕获的新增输出）。
    CmdOutput { output: String },
    /// 危险命令确认结果：true=执行该条 / false=跳过该条。
    Confirm { run: bool },
    /// 用户点了「停止」。
    Stop,
    /// 锁定的终端窗格死亡（连接断开 / 标签关闭）。
    TargetDied,
}

/// 状态机输出动作（app 层逐条执行）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentAction {
    /// 往面板写一条消息（进度 / 总结 / 错误）。
    Say(String),
    /// 向锁定的终端写命令（app 层补 \r、等输出静默、截新增输出）。
    RunCommand(String),
    /// 危险命令：暂停整轮，弹确认卡。
    AskConfirm(String),
    /// 发起下一轮模型请求（prompt 已按历史组装好）。
    RequestModelTurn(String),
    /// 正常结束（summary）。
    Finish(String),
    /// 异常中止（终端死亡等）。
    Abort(String),
}

/// 状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentState {
    /// 等模型回复（step = 已发出的模型请求数）。
    Thinking { step: usize },
    /// 命令已写入终端，等输出。
    Running { step: usize, cmd: String },
    /// 危险命令待确认（本轮剩余命令排在 pending 里）。
    WaitingConfirm { step: usize, cmd: String },
    /// 结束。
    Done,
}

/// Agent 执行循环（纯数据，驱动在 app 层）。
pub struct AgentLoop {
    pub state: AgentState,
    pub task: String,
    /// 已执行的 (命令, 输出) 清单。
    pub executed: Vec<(String, String)>,
    /// 每条命令的执行次数（去重上限用，按 trim 后全文计）。
    run_counts: HashMap<String, usize>,
    /// 本轮剩余命令（模型一回复可能给多条）。
    pending: VecDeque<String>,
    /// 喂模型的历史。
    pub history: Vec<HistoryEntry>,
    /// 最后一次跑的命令（状态行显示用）。
    pub last_cmd: Option<String>,
}

impl AgentLoop {
    /// 开始一个任务。返回初始动作（发第一轮模型请求）。
    pub fn start(task: String, snapshot: String) -> (Self, Vec<AgentAction>) {
        let mut this = Self {
            state: AgentState::Thinking { step: 1 },
            task: task.clone(),
            executed: Vec::new(),
            run_counts: HashMap::new(),
            pending: VecDeque::new(),
            history: Vec::new(),
            last_cmd: None,
        };
        this.history.push(HistoryEntry {
            role: "user".into(),
            content: format!(
                "[Agent] 任务：{task}\n当前终端输出（最近若干行）：\n{snapshot}"
            ),
        });
        let prompt = this.model_prompt();
        (
            this,
            vec![
                AgentAction::Say(format!("🤖 Agent 开始：{task}")),
                AgentAction::RequestModelTurn(prompt),
            ],
        )
    }

    /// 组装下一轮模型请求的 prompt（把执行历史喂回去）。
    fn model_prompt(&self) -> String {
        let mut prompt = String::from("[Agent] 继续任务。");
        for (cmd, output) in &self.executed {
            prompt.push_str(&format!(
                "\n\n【已执行】{cmd}\n【输出】\n{}",
                if output.is_empty() { "（无输出）" } else { output }
            ));
        }
        prompt.push_str("\n\n请给出下一步（bash 代码块），任务完成则在回复末尾加 [DONE]。");
        prompt
    }

    /// 喂模型的历史（app 层转 `AiMessage` 后调 `ai::chat`）。
    pub fn chat_history(&self) -> Vec<HistoryEntry> {
        self.history.clone()
    }

    /// 当前状态行的文本（面板顶栏显示）。
    pub fn status_line(&self) -> String {
        let step = match &self.state {
            AgentState::Thinking { step } => *step,
            AgentState::Running { step, .. } => *step,
            AgentState::WaitingConfirm { step, .. } => *step,
            AgentState::Done => self.history.len(),
        };
        let last = self
            .last_cmd
            .as_ref()
            .map(|c| format!(" · 上一条 `{c}`"))
            .unwrap_or_default();
        format!("Agent：步骤 {step}/{MAX_STEPS}{last}")
    }

    /// 推进状态机。返回要执行的动作序列（app 按序执行）。
    pub fn next(&mut self, ev: AgentEvent) -> Vec<AgentAction> {
        // 停止在任何状态下都立即终止（已发出的命令不撤回）。
        if matches!(ev, AgentEvent::Stop) {
            self.state = AgentState::Done;
            return vec![AgentAction::Finish("已按用户要求停止本轮 Agent。".into())];
        }
        if matches!(ev, AgentEvent::TargetDied) {
            self.state = AgentState::Done;
            return vec![AgentAction::Abort("终端窗格已断开，本轮 Agent 终止。".into())];
        }

        match (std::mem::replace(&mut self.state, AgentState::Done), ev) {
            // ---- 等模型回复 ----
            (
                AgentState::Thinking { step },
                AgentEvent::ModelReply {
                    plan:
                        crate::ai::AgentPlan {
                            think,
                            commands,
                            done,
                            summary,
                        },
                },
            ) => {
                self.history.push(HistoryEntry {
                    role: "assistant".into(),
                    content: format!(
                        "think={think} commands={commands:?} done={done} {}",
                        summary.clone().unwrap_or_default()
                    ),
                });
                // 模型的推理/说明（非空时）作为进度气泡展示（JSON think /
                // bash 降级时的块外正文）。
                let mut actions = Vec::new();
                if !think.is_empty() {
                    actions.push(AgentAction::Say(think.clone()));
                }
                if done {
                    self.state = AgentState::Done;
                    if !self.executed.is_empty() {
                        let list = self
                            .executed
                            .iter()
                            .map(|(c, _)| format!("`{c}`"))
                            .collect::<Vec<_>>()
                            .join("、");
                        actions.push(AgentAction::Say(format!("本轮执行过的命令：{list}")));
                    }
                    // summary：JSON 协议的字段；bash 协议没有 summary 字段，
                    // 块外正文（think）就是总结——直接作为 Finish 内容。
                    let summary = summary
                        .filter(|s| !s.is_empty())
                        .or_else(|| (!think.is_empty()).then_some(think))
                        .unwrap_or_else(|| "任务完成。".into());
                    actions.push(AgentAction::Finish(summary));
                    return actions;
                }
                // 步数上限：第 MAX_STEPS 轮还不是 done 就收尾。
                if step >= MAX_STEPS {
                    self.state = AgentState::Done;
                    actions.push(AgentAction::Finish(format!(
                        "已达 Agent 步数上限（{MAX_STEPS} 轮），自动停止。"
                    )));
                    return actions;
                }

                // 命令队列：裁到单轮上限 + 去重（同命令 >2 次的丢弃）。
                let mut queue: VecDeque<String> = commands.into_iter().collect();
                while queue.len() > MAX_COMMANDS_PER_TURN {
                    queue.pop_back();
                }
                queue.retain(|cmd| {
                    let n = self.run_counts.get(cmd.trim()).copied().unwrap_or(0);
                    if n >= MAX_SAME_COMMAND_RUNS {
                        self.history.push(HistoryEntry {
                            role: "user".into(),
                            content: format!(
                                "[Agent] 命令 `{cmd}` 已重复执行 {n} 次，本轮跳过（防死循环）。"
                            ),
                        });
                        return false;
                    }
                    true
                });

                if queue.is_empty() {
                    // 模型没给可执行命令：降级为普通展示并结束（不崩、不空转）。
                    // 优先用模型的 summary/think 说明原因。
                    self.state = AgentState::Done;
                    let note = summary.or(Some(think))
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "模型未给出可执行命令，本轮结束。".into());
                    actions.push(AgentAction::Finish(note));
                    return actions;
                }

                self.pending = queue;
                self.state = AgentState::Thinking { step };
                actions.extend(self.dispatch_next(step));
                actions
            }

            // ---- 命令在终端跑 ----
            (AgentState::Running { step, cmd }, AgentEvent::CmdOutput { output }) => {
                self.executed.push((cmd.clone(), output.clone()));
                self.history.push(HistoryEntry {
                    role: "user".into(),
                    content: format!(
                        "[Agent] 第 {step} 轮命令 `{cmd}` 的输出：\n{}",
                        if output.is_empty() { "（无输出）" } else { &output }
                    ),
                });
                self.last_cmd = Some(cmd);
                // 本轮还有命令就继续派发；否则进入下一轮模型请求。
                if !self.pending.is_empty() {
                    self.state = AgentState::Thinking { step };
                    self.dispatch_next(step)
                } else {
                    let next_step = step + 1;
                    if next_step > MAX_STEPS {
                        self.state = AgentState::Done;
                        return vec![AgentAction::Finish(format!(
                            "已达 Agent 步数上限（{MAX_STEPS} 轮），自动停止。"
                        ))];
                    }
                    self.state = AgentState::Thinking { step: next_step };
                    let prompt = self.model_prompt();
                    vec![AgentAction::RequestModelTurn(prompt)]
                }
            }

            // ---- 危险命令确认 ----
            (AgentState::WaitingConfirm { step, cmd }, AgentEvent::Confirm { run: true }) => {
                self.state = AgentState::Running { step, cmd: cmd.clone() };
                vec![AgentAction::RunCommand(cmd)]
            }
            (AgentState::WaitingConfirm { step, .. }, AgentEvent::Confirm { run: false }) => {
                self.state = AgentState::Thinking { step };
                let skipped = AgentAction::Say("已跳过该危险命令。".into());
                let mut actions = vec![skipped];
                actions.extend(self.dispatch_next(step));
                actions
            }

            // ---- 不配套的事件（防御）：保持当前状态不变 ----
            (state, ev) => {
                self.state = state;
                let state = self.state.clone();
                vec![AgentAction::Abort(format!(
                    "Agent 状态机收到与当前状态 ({state:?}) 不匹配的事件 ({ev:?})，已忽略。"
                ))]
            }
        }
    }

    /// 派发 pending 队首：危险 → AskConfirm 暂停；安全 → 直接执行。
    /// 队列空 → 发起下一轮模型请求。
    fn dispatch_next(&mut self, step: usize) -> Vec<AgentAction> {
        let Some(cmd) = self.pending.pop_front() else {
            // 本轮命令跑完 → 下一轮模型请求
            let next_step = step + 1;
            if next_step > MAX_STEPS {
                self.state = AgentState::Done;
                return vec![AgentAction::Finish(format!(
                    "已达 Agent 步数上限（{MAX_STEPS} 轮），自动停止。"
                ))];
            }
            self.state = AgentState::Thinking { step: next_step };
            let prompt = self.model_prompt();
            return vec![AgentAction::RequestModelTurn(prompt)];
        };
        if ai_is_dangerous(&cmd) {
            self.state = AgentState::WaitingConfirm { step, cmd: cmd.clone() };
            vec![AgentAction::AskConfirm(cmd)]
        } else {
            self.state = AgentState::Running { step, cmd: cmd.clone() };
            *self.run_counts.entry(cmd.trim().to_string()).or_insert(0) += 1;
            vec![AgentAction::RunCommand(cmd)]
        }
    }
}

/// 危险命令检测（转发到 `ai::is_dangerous`，保持单一事实来源）。
fn ai_is_dangerous(cmd: &str) -> bool {
    crate::ai::is_dangerous(cmd)
}

#[cfg(test)]
mod tests {
    // 本文件无 gpui_kit 通配导入，`use super::*` 安全。
    use super::*;

    fn start() -> (AgentLoop, Vec<AgentAction>) {
        AgentLoop::start(
            "查看当前目录下最大的 3 个文件并按大小排序".into(),
            "user@host:~$ ".into(),
        )
    }

    /// 组一条模型回复事件：文本经 `ai::parse_agent_reply` **双格式解析**后
    /// 构造（fixture 同时充当解析器的集成测试）。
    fn reply(text: &str) -> AgentEvent {
        AgentEvent::ModelReply {
            plan: crate::ai::parse_agent_reply(text),
        }
    }

    /// 正常两步任务走通：第一轮给命令，第二轮 done(summary)。
    /// （fixture 用 bash 协议——弱模型形态；JSON 形态在 ai.rs 的解析测试里。）
    #[test]
    fn two_step_task_completes() {
        let (mut loop_, actions) = start();
        assert!(actions
            .iter()
            .any(|a| matches!(a, AgentAction::RequestModelTurn(_))));

        let actions = loop_.next(reply("```bash\ndu -a .\n```\n```bash\nsort -n\n```"));
        assert_eq!(actions[0], AgentAction::RunCommand("du -a .".into()));
        // 单轮 2 条 → 第二条也直接派发（cmd dispatch 在 CmdOutput 后）
        assert_eq!(actions.len(), 1);

        let actions = loop_.next(AgentEvent::CmdOutput { output: "total".into() });
        assert_eq!(
            actions,
            vec![AgentAction::RunCommand("sort -n".into())]
        );

        let actions = loop_.next(AgentEvent::CmdOutput { output: "sorted".into() });
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, AgentAction::RequestModelTurn(_))),
            "本轮命令跑完应发起下一轮模型请求"
        );

        let actions = loop_.next(reply("最大的文件是 a.tar，1.2G [DONE]"));
        // 结束动作：think/正文（Say）+ 执行清单（Say）+ summary（Finish）
        assert!(actions
            .iter()
            .any(|a| matches!(a, AgentAction::Say(s) if s.contains("`du -a .`") && s.contains("`sort -n`"))));
        assert!(actions.contains(&AgentAction::Finish("最大的文件是 a.tar，1.2G".into())));
        assert_eq!(loop_.state, AgentState::Done);
    }

    /// 危险命令 → ModelReply 当轮即 AskConfirm 暂停（不写入终端）。
    #[test]
    fn dangerous_command_requires_confirmation() {
        let (mut loop_, _) = start();
        let actions = loop_.next(reply("```bash\nrm -rf /tmp/x\n```"));
        assert!(
            matches!(actions.as_slice(), [AgentAction::AskConfirm(cmd)] if cmd == "rm -rf /tmp/x"),
            "危险命令必须暂停整轮并弹确认（派发即暂停，不写终端）"
        );
        assert!(matches!(loop_.state, AgentState::WaitingConfirm { .. }));

        // 跳过 → 队列空 → 下一轮模型请求
        let actions = loop_.next(AgentEvent::Confirm { run: false });
        assert!(actions.contains(&AgentAction::Say("已跳过该危险命令。".into())));
        assert!(actions
            .iter()
            .any(|a| matches!(a, AgentAction::RequestModelTurn(_))));
    }

    #[test]
    fn dangerous_command_confirmed_runs() {
        let (mut loop_, _) = start();
        let actions = loop_.next(reply("```bash\nshutdown -h now\n```"));
        assert!(matches!(actions.as_slice(), [AgentAction::AskConfirm(_)]));

        let actions = loop_.next(AgentEvent::Confirm { run: true });
        assert_eq!(
            actions,
            vec![AgentAction::RunCommand("shutdown -h now".into())]
        );
    }

    /// 用户「停止」→ 立即终止（任何状态），不再发任何命令。
    #[test]
    fn stop_terminates_from_any_state() {
        let setups: [(fn(&mut AgentLoop), &str); 2] = [
            (
                |l: &mut AgentLoop| {
                    l.next(reply("```bash\nls\n```"));
                },
                "Running 状态",
            ),
            (
                |l: &mut AgentLoop| {
                    l.next(reply("```bash\nrm -rf /\n```"));
                    l.next(AgentEvent::CmdOutput { output: String::new() });
                },
                "WaitingConfirm 状态",
            ),
        ];
        for (state_setup, desc) in setups {
            let (mut loop_, _) = start();
            state_setup(&mut loop_);
            let actions = loop_.next(AgentEvent::Stop);
            assert!(
                actions.contains(&AgentAction::Finish("已按用户要求停止本轮 Agent。".into())),
                "{desc}"
            );
            assert_eq!(loop_.state, AgentState::Done);
        }
    }

    /// 步数上限：第 10 轮模型仍不给 done → 自动收尾。
    /// （命令每轮不同，避开同命令去重。）
    #[test]
    fn step_limit_terminates() {
        let (mut loop_, _) = start();
        for i in 0..MAX_STEPS {
            let cmd = format!("ls /step{i}");
            let actions = loop_.next(reply(&format!("```bash\n{cmd}\n```")));
            // 最后一轮（第 10 轮回复仍非 done）必须 Finish
            if i + 1 == MAX_STEPS {
                assert!(matches!(actions.as_slice(),
                    [AgentAction::Finish(m)] if m.contains("步数上限")));
                assert_eq!(loop_.state, AgentState::Done);
                return;
            }
            assert!(actions.iter().any(|a| matches!(a, AgentAction::RunCommand(_))));
            loop_.next(AgentEvent::CmdOutput { output: String::new() });
        }
        panic!("未在步数上限处终止");
    }

    /// 同一条命令不重复超 2 次：第 3 次出现被丢弃。
    #[test]
    fn repeated_command_is_dropped() {
        let (mut loop_, _) = start();
        // 第 1 次
        let actions = loop_.next(reply("```bash\napt install x\n```"));
        assert!(actions.iter().any(|a| matches!(a, AgentAction::RunCommand(_))));
        loop_.next(AgentEvent::CmdOutput { output: "fail".into() });
        // 第 2 次
        let actions = loop_.next(reply("```bash\napt install x\n```"));
        assert!(actions.iter().any(|a| matches!(a, AgentAction::RunCommand(_))));
        loop_.next(AgentEvent::CmdOutput { output: "fail".into() });
        // 第 3 次 → 被丢弃且没有可执行命令 → 降级 Finish
        let actions = loop_.next(reply("```bash\napt install x\n```"));
        assert!(matches!(actions.as_slice(), [AgentAction::Finish(m)] if m.contains("模型未给出可执行命令")));
        assert_eq!(loop_.state, AgentState::Done);
    }

    /// 单轮命令数上限：模型给 5 条只执行前 3 条。
    #[test]
    fn per_turn_command_clamp() {
        let (mut loop_, _) = start();
        let actions = loop_.next(
            reply("```bash\na\n```\n```bash\nb\n```\n```bash\nc\n```\n```bash\nd\n```\n```bash\ne\n```"),
        );
        let run = actions
            .iter()
            .filter(|a| matches!(a, AgentAction::RunCommand(_)))
            .count();
        assert_eq!(run, 1, "一次只派发队首一条（其余排队）");
        // 队列里应只剩 2 条（5 → clamp 3 → 派 1）
        let out = loop_.next(AgentEvent::CmdOutput { output: String::new() });
        assert!(matches!(out.as_slice(), [AgentAction::RunCommand(_)]));
        let out = loop_.next(AgentEvent::CmdOutput { output: String::new() });
        assert!(matches!(out.as_slice(), [AgentAction::RunCommand(_)]));
        // 第 3 条跑完 → 队列空 → 下一轮模型请求
        let out = loop_.next(AgentEvent::CmdOutput { output: String::new() });
        assert!(out.iter().any(|a| matches!(a, AgentAction::RequestModelTurn(_))));
    }

    /// 终端窗格死亡 → Abort。
    #[test]
    fn target_death_aborts() {
        let (mut loop_, _) = start();
        let actions = loop_.next(AgentEvent::TargetDied);
        assert!(matches!(actions.as_slice(),
            [AgentAction::Abort(m)] if m.contains("终端窗格已断开")));
        assert_eq!(loop_.state, AgentState::Done);
    }

    /// 模型没给命令也没 done（空回复）→ 降级结束，不空转。
    #[test]
    fn empty_reply_degrades_to_finish() {
        let (mut loop_, _) = start();
        let actions = loop_.next(reply(""));
        assert!(matches!(actions.as_slice(),
            [AgentAction::Finish(m)] if m.contains("模型未给出可执行命令")));
        assert_eq!(loop_.state, AgentState::Done);
    }

    /// JSON plan（强模型形态）走同一个状态机：think 变进度气泡、commands
    /// 照常派发（安全闸一致）。
    #[test]
    fn json_plan_drives_the_same_loop() {
        let (mut loop_, _) = start();
        let actions = loop_.next(reply(
            "让我看看。\n```json\n{\"think\": \"先列目录\", \"commands\": [\"ls\"], \"done\": false}\n```",
        ));
        assert!(actions.contains(&AgentAction::Say("先列目录".into())));
        assert!(actions.iter().any(|a| matches!(a, AgentAction::RunCommand(_))));

        let actions = loop_.next(AgentEvent::CmdOutput { output: String::new() });
        assert!(actions
            .iter()
            .any(|a| matches!(a, AgentAction::RequestModelTurn(_))));

        let actions = loop_.next(reply(
            "```json\n{\"think\": \"ok\", \"done\": true, \"summary\": \"已列出\"}\n```",
        ));
        assert!(actions.contains(&AgentAction::Finish("已列出".into())));
    }

    /// 状态行：包含步骤计数与最后一条命令。
    /// （步骤计数 = 已发出的模型请求数：第 1 轮命令跑完、即将请求第 2 轮时
    /// 显示 2/10。）
    #[test]
    fn status_line_reports_step_and_last_cmd() {
        let (mut loop_, _) = start();
        loop_.next(reply("```bash\nls -la\n```"));
        loop_.next(AgentEvent::CmdOutput { output: String::new() });
        let line = loop_.status_line();
        assert!(line.contains("2/10"), "状态行应有步骤计数：{line}");
        assert!(line.contains("ls -la"), "状态行应有最后一条命令：{line}");
    }
}

/// Agent 步骤结算判定（纯函数，可单测）：命令写入后等「输出静默」而不是
/// 固定时长——固定 800ms 会把慢命令（find / 大文件操作 / yum）的输出半途
/// 截断喂给模型。
///
/// - `since_write`：命令写入终端至今
/// - `since_last_change`：终端输出快照最近一次变化至今
///
/// 规则：静默 ≥1.2s 且写入 ≥0.4s → 结算；写入 ≥20s 硬上限 → 强制结算
/// （持续输出型命令如 tail -f 截取部分输出，由系统提示词告知模型可能不完整）。
pub fn agent_should_settle(
    since_write: std::time::Duration,
    since_last_change: std::time::Duration,
) -> bool {
    const MIN_WAIT: std::time::Duration = std::time::Duration::from_millis(400);
    const SILENCE: std::time::Duration = std::time::Duration::from_millis(1200);
    const MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(20);
    if since_write >= MAX_WAIT {
        return true;
    }
    since_last_change >= SILENCE && since_write >= MIN_WAIT
}

#[cfg(test)]
mod settle_tests {
    use super::agent_should_settle;
    use std::time::Duration;

    #[test]
    fn fast_command_settles_after_silence() {
        // 快命令：写入 0.5s、静默 1.3s → 结算
        assert!(agent_should_settle(
            Duration::from_millis(500),
            Duration::from_millis(1300)
        ));
    }

    #[test]
    fn active_output_keeps_waiting() {
        // 慢命令：输出持续到达（距上次变化仅 0.2s）→ 不结算
        assert!(!agent_should_settle(
            Duration::from_millis(2000),
            Duration::from_millis(200)
        ));
    }

    #[test]
    fn min_wait_blocks_instant_settle() {
        // 刚写入（回显未落）即使无输出变化也不结算
        assert!(!agent_should_settle(
            Duration::from_millis(100),
            Duration::from_millis(100)
        ));
    }

    #[test]
    fn max_wait_forces_settle_for_endless_output() {
        // tail -f 类持续输出：20s 硬上限强制结算
        assert!(agent_should_settle(
            Duration::from_secs(25),
            Duration::from_millis(50)
        ));
    }
}

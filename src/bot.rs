//! BOT：起一个 headless 进程、收它的回答、管它的生死。
//!
//! ## 一次调用的完整生命周期（每一步都要有日志）
//!
//! ```text
//! 请求进来
//!   → bot.start      记 name/team/pid
//!   → 读 .session     有就 --session-id 续上
//!   → spawn dsh
//!   → 逐行解析 --json 事件流（session / final）
//!   → bot.finish     记 ok/ms；或 bot.timeout / bot.no_answer
//!   → 写回 session
//! ```
//!
//! ## 为什么不用 `wait_with_output()`
//!
//! 那样得等进程退出才拿到全部输出，超时就没法中断。
//! 这里逐行读 stdout，**到点就能 kill**，而且能实时看到 session id。

use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::error::DaemonError;
use crate::team::TeamStore;
use crate::{log_debug, log_error, log_info, log_warn};

pub struct RunOutcome {
    pub reply: String,
    pub session_id: Option<String>,
    pub pid: u32,
    pub ms: u64,
    pub continued: bool,
}

/// 跑一个 BOT，等它答完。失败返回结构化错误。
#[allow(clippy::too_many_arguments)]
pub fn run(
    teams: &TeamStore,
    agents_dir: &Path,
    name: &str,
    message: &str,
    team_id: Option<&str>,
    sender: Option<&str>,
    timeout_ms: u64,
    // spawn 成功后立刻回报真实 PID —— 身份反查要靠它，不能是 0
    on_spawn: &dyn Fn(u32),
) -> Result<RunOutcome, DaemonError> {
    let agent_dir = agents_dir.join(name);
    if !agent_dir.is_dir() {
        let available = list_available(agents_dir);
        return Err(DaemonError::BotNotFound {
            name: name.to_string(),
            available,
        });
    }

    let sess_path = teams.session_path(agents_dir, team_id, name);
    let existing = teams.read_session(&sess_path);

    let mut cmd = Command::new("dsh");
    cmd.arg("--profile").arg(name).arg("--json");
    if let Some(sid) = &existing {
        cmd.arg("--session-id").arg(sid);
    }
    // 长期记忆：跨 Team 共享的那份（D003）。人设里没有它，它是「学到的经验」。
    // 短期记忆（会话）在 teams/<id>/sessions/<bot>.session，两份互不干扰。
    let memory_note = read_long_term_memory(&agent_dir);
    // 消息头：告诉它「谁在问、在哪个 Team」。
    //
    // 这不是环境变量、也不是配置文件 —— 是**调用现场的事实**：
    // 谁发起的、属于哪个 Team，由 daemon 从 PID 反查得到，写在这一句里。
    let header = match (sender, team_id) {
        (Some(from), Some(team)) => format!("[来自 {} · Team {}]\n", from, team),
        (Some(from), None) => format!("[来自 {}]\n", from),
        (None, Some(team)) => format!("[Team {}]\n", team),
        (None, None) => String::new(),
    };

    let full_message = match &memory_note {
        Some(m) if !m.trim().is_empty() => format!(
            "{}{}\n\n---\n（以下是你自己的长期记忆，跨任务共享；仅供参考，与本次任务冲突时以本次为准）\n{}",
            header, message, m
        ),
        _ => format!("{}{}", header, message),
    };
    cmd.arg(&full_message);
    cmd.current_dir(&agent_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let started = Instant::now();
    let mut child: Child = cmd.spawn().map_err(|e| {
        log_error!("bot.spawn_failed", bot = name, error = &e);
        DaemonError::SpawnFailed {
            name: name.to_string(),
            reason: e.to_string(),
        }
    })?;
    let pid = child.id();
    on_spawn(pid);

    log_info!(
        "bot.start",
        bot = name,
        pid = pid,
        team = team_id.unwrap_or("-"),
        resumed = existing.is_some()
    );

    // stderr 独立线程收集，避免管道写满阻塞
    let stderr_handle = child.stderr.take().map(|mut e| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = e.read_to_string(&mut buf);
            buf
        })
    });

    let mut session_id: Option<String> = None;
    let mut final_text: Option<String> = None;
    let mut timed_out = false;

    if let Some(out) = child.stdout.take() {
        let reader = BufReader::new(out);
        let deadline = started + Duration::from_millis(timeout_ms);
        for line in reader.lines() {
            if Instant::now() > deadline {
                timed_out = true;
                let _ = child.kill();
                break;
            }
            let Ok(line) = line else { break };
            let trimmed = line.trim();
            if !trimmed.starts_with('{') {
                continue;
            }
            match serde_json::from_str::<Value>(trimmed) {
                Ok(evt) => match evt.get("type").and_then(|t| t.as_str()) {
                    Some("session") => {
                        if let Some(sid) = evt.get("sessionId").and_then(|v| v.as_str()) {
                            session_id = Some(sid.to_string());
                            log_debug!("bot.session", bot = name, session = sid);
                        }
                    }
                    Some("final") => {
                        if let Some(txt) = evt.get("text").and_then(|v| v.as_str()) {
                            final_text = Some(txt.to_string());
                        }
                    }
                    _ => {}
                },
                Err(e) => log_debug!("bot.event_parse_failed", bot = name, error = e, raw = trimmed),
            }
        }
    }

    let status = child.wait().ok();
    let ms = started.elapsed().as_millis() as u64;
    let stderr_text = stderr_handle.and_then(|h| h.join().ok()).unwrap_or_default();

    if let Some(sid) = &session_id {
        teams.write_session(&sess_path, sid);
    }

    if timed_out || final_text.is_none() && ms >= timeout_ms {
        log_warn!("bot.timeout", bot = name, timeout_ms = timeout_ms, ms = ms);
        return Err(DaemonError::BotTimeout {
            name: name.to_string(),
            timeout_ms,
        });
    }

    match final_text {
        Some(reply) => {
            log_info!("bot.finish", bot = name, ok = true, ms = ms, chars = reply.chars().count());
            let continued = session_id.is_some();
            Ok(RunOutcome {
                reply,
                session_id: session_id.or(existing),
                pid,
                ms,
                continued,
            })
        }
        None => {
            let tail: Vec<&str> = stderr_text.lines().rev().take(3).collect();
            let tail: Vec<&str> = tail.into_iter().rev().collect();
            let tail_str = tail.join(" | ");
            log_error!(
                "bot.no_answer",
                bot = name,
                exit_code = status.and_then(|s| s.code()).map(|c| c.to_string()).unwrap_or_default(),
                stderr = &tail_str
            );
            Err(DaemonError::BotNoAnswer {
                name: name.to_string(),
                exit_code: status.and_then(|s| s.code()),
                stderr_tail: tail_str,
            })
        }
    }
}

/// 读长期记忆文件：`~/.dsh/agents/<bot>/memory.md`。
///
/// **为什么是文件而不是会话**：
///   - 会话按 Team 隔离（D009），跨任务不通
///   - 长期记忆要跨任务共享（D003），所以放在 Team 之外
///   - 用人可读的 markdown，用户能直接改，BOT 也能自己追加
///
/// **为什么不塞进 AGENTS.md**：那是人设（身份），改它等于改它这个人。
/// 记忆是「经历」，两者不能混。
fn read_long_term_memory(agent_dir: &Path) -> Option<String> {
    let path = agent_dir.join("memory.md");
    let raw = std::fs::read_to_string(path).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// 扫一遍有哪些 BOT（给错误提示用）。
pub fn list_available(agents_dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(agents_dir) else {
        return out;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                out.push(name.to_string());
            }
        }
    }
    out.sort();
    out
}

/// 各 BOT 的状态快照。
pub fn snapshot(
    agents_dir: &Path,
    team_filter: Option<&str>,
    running: &[(String, u32, Option<String>)],
) -> Value {
    let mut bots: Vec<Value> = Vec::new();
    let Ok(rd) = std::fs::read_dir(agents_dir) else {
        return json!({ "bots": bots });
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let active = running.iter().find(|(n, _, team)| {
            n == name
                && team_filter
                    .map(|t| team.as_deref() == Some(t))
                    .unwrap_or(true)
        });
        bots.push(json!({
            "name": name,
            "status": if active.is_some() { "working" } else { "idle" },
            "pid": active.map(|(_, pid, _)| *pid),
            "teamId": active.and_then(|(_, _, t)| t.clone()),
        }));
    }
    bots.sort_by(|a, b| {
        a.get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .cmp(b.get("name").and_then(|v| v.as_str()).unwrap_or(""))
    });
    json!({ "bots": bots })
}

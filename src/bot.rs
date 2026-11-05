//! BOT：起一个 headless 进程、收它的回答、管它的生死。
//!
//! ## 一次调用的完整生命周期（每一步都要有日志）
//!
//! ```text
//! 请求进来
//!   → bot.start      记 name/group/pid
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

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::error::DaemonError;
use crate::group::GroupStore;
use crate::{log_debug, log_error, log_info, log_warn};

/// 中文名 → 目录名 的缓存。BOT 名单在运行时不会变，扫一次就够。
fn alias_cache() -> &'static Mutex<Option<HashMap<String, String>>> {
    static CACHE: OnceLock<Mutex<Option<HashMap<String, String>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// 从 `AGENTS.md` 首行取中文名。格式：`# 苏见微（本地检索 Agent）`。
fn display_name_from(agents_dir: &Path, bot: &str) -> Option<String> {
    let head = std::fs::read_to_string(agents_dir.join(bot).join("AGENTS.md")).ok()?;
    let first = head.lines().next()?.trim().to_string();
    let rest = first.strip_prefix("# ")?.to_string();
    // 砍掉「（角色）」后缀
    let cut = rest.split(['（', '(']).next()?.trim().to_string();
    if cut.is_empty() {
        None
    } else {
        Some(cut)
    }
}

/// 把中文名解析成目录名。找不到就原样返回。
///
/// 为什么需要：`agents_dir.join("钟诗雨")` 这个目录不存在，
/// 于是调用方拿到 BOT_NOT_FOUND。实测苏见微 7 次调用里 4 次用了中文名，
/// 每次都要先失败一次才知道该用 `zhongshiyu`。
pub fn resolve_alias(agents_dir: &Path, name: &str) -> String {
    let direct = agents_dir.join(name);
    if direct.is_dir() {
        return name.to_string();
    }
    // 只在名字里含非 ASCII 时才有必要扫；纯英文名走原路径，省一次全目录扫描
    if name.is_ascii() {
        return name.to_string();
    }

    let mut guard = match alias_cache().lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if guard.is_none() {
        let mut map = HashMap::new();
        if let Ok(entries) = std::fs::read_dir(agents_dir) {
            for entry in entries.flatten() {
                if !entry.path().is_dir() {
                    continue;
                }
                let Some(bot) = entry.file_name().to_str().map(|s| s.to_string()) else {
                    continue;
                };
                if let Some(display) = display_name_from(agents_dir, &bot) {
                    map.insert(display, bot);
                }
            }
        }
        *guard = Some(map);
    }
    let map = guard.as_ref().expect("filled above");
    map.get(name).cloned().unwrap_or_else(|| name.to_string())
}

pub struct RunOutcome {
    /// 解析后的目录名（中文名进来时会被换成 zhongshiyu 这类）
    pub bot: String,
    pub reply: String,
    pub session_id: Option<String>,
    pub pid: u32,
    pub ms: u64,
    pub continued: bool,
}

/// 跑一个 BOT，等它答完。失败返回结构化错误。
#[allow(clippy::too_many_arguments)]
pub fn run(
    groups: &GroupStore,
    agents_dir: &Path,
    name: &str,
    message: &str,
    group_id: Option<&str>,
    sender: Option<&str>,
    timeout_ms: u64,
    // spawn 成功后立刻回报真实 PID —— 身份反查要靠它，不能是 0
    on_spawn: &dyn Fn(u32),
) -> Result<RunOutcome, DaemonError> {
    // 中文名 → 目录名（「钟诗雨」→ zhongshiyu）。解析不到就原样往下走，
    // 让下面那句 is_dir() 去报 BOT_NOT_FOUND —— 错误信息里带可用名单。
    let resolved = resolve_alias(agents_dir, name);
    let name: &str = &resolved;

    let agent_dir = agents_dir.join(name);
    if !agent_dir.is_dir() {
        let available = list_available(agents_dir);
        return Err(DaemonError::BotNotFound {
            name: name.to_string(),
            available,
        });
    }

    let sess_path = groups.session_path(agents_dir, group_id, name);
    let existing = groups.read_session(&sess_path);

    let mut cmd = Command::new("dsh");
    cmd.arg("--profile").arg(name).arg("--json");
    if let Some(sid) = &existing {
        cmd.arg("--session-id").arg(sid);
    }
    // 长期记忆：跨 群 共享的那份（D003）。人设里没有它，它是「学到的经验」。
    // 短期记忆（会话）在 teams/<id>/sessions/<bot>.session，两份互不干扰。
    let memory_note = read_long_term_memory(&agent_dir);
    // 消息头：告诉它「谁在问、在哪个 Team」。
    //
    // 这不是环境变量、也不是配置文件 —— 是**调用现场的事实**：
    // 谁发起的、属于哪个 Team，由 daemon 从 PID 反查得到，写在这一句里。
    // 群工作区的**绝对路径**——必须写进消息头。
    //
    // 为什么：BOT 的 cwd 是 `~/.dsh/agents/<bot>/`（人设在那，dsh 靠 cwd 找 AGENTS.md）。
    // 任务书里写「workspace/deliverables/x.md」这种相对路径，它会老老实实写进
    // `agents/<bot>/workspace/deliverables/` —— **群的工作区从头到尾没被用过**
    // （实测抓到：钟诗雨说 done，群目录里没有那个文件，文件在它自己家）。
    let workspace = group_id.map(|gid| {
        let root = std::env::var("DSH_HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| {
                std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".dsh")
            });
        root.join("groups").join(gid).join("workspace")
    });

    let header = match (sender, group_id, &workspace) {
        (Some(from), Some(team), Some(ws)) => format!(
            "[来自 {} · 群 {}]\n[群工作区（绝对路径，交付物写这里）：{}]\n",
            from,
            team,
            ws.display()
        ),
        (Some(from), Some(team), None) => format!("[来自 {} · 群 {}]\n", from, team),
        (Some(from), None, _) => format!("[来自 {}]\n", from),
        (None, Some(team), Some(ws)) => format!(
            "[群 {}]\n[群工作区（绝对路径，交付物写这里）：{}]\n",
            team,
            ws.display()
        ),
        (None, Some(team), None) => format!("[群 {}]\n", team),
        (None, None, _) => String::new(),
    };

    let full_message = match &memory_note {
        Some(m) if !m.trim().is_empty() => format!(
            "{}{}\n\n---\n（以下是你自己的长期记忆，跨任务共享；仅供参考，与本次任务冲突时以本次为准）\n{}",
            header, message, m
        ),
        _ => format!("{}{}", header, message),
    };
    cmd.arg(&full_message);

    // 告诉这个 BOT「你在哪个群」。
    //
    // 为什么必须传：插件要按会话 resolve 群，而 **BOT 自己的 session 不是主会话**——
    // 不告诉它，它就会 resolve 出一个新群，然后 group_whoami 报错群、
    // bot_escalate 把求援写进一个没人看的空群（实测抓到过）。
    if let Some(gid) = group_id {
        cmd.env("DSH_BOT_GROUP_ID", gid);
    }
    if let Some(from) = sender {
        cmd.env("DSH_BOT_SENDER", from);
    }
    cmd.env("DSH_BOT_NAME", name);

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
        team = group_id.unwrap_or("-"),
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
        groups.write_session(&sess_path, sid);
    }

    // 派活即入群（D028）已经移到 spawn 之前做（main.rs）——
    // 放在这里会让 BOT 干活时查 group_whoami **看不见自己**（实测抓到）。
    if let Some(gid) = group_id {
        let ok = final_text.is_some() && !timed_out;
        groups.append_event(
            gid,
            json!({
                "at": crate::group::now_stamp(),
                "event": if ok { "task.returned" } else { "task.failed" },
                "group": gid,
                "bot": name,
                "from": sender,
                "ms": ms,
                "replyChars": final_text.as_ref().map(|r| r.chars().count()).unwrap_or(0),
                "session": session_id.as_deref(),
            }),
        );
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
                bot: name.to_string(),
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
///   - 会话按 群 隔离（D009），跨任务不通
///   - 长期记忆要跨任务共享（D003），所以放在 群 之外
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
            "groupId": active.and_then(|(_, _, t)| t.clone()),
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

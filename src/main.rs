//! dsh-bot-daemon —— BOT 进程池的看门人（装配层）。
//!
//! ## 它是什么
//!
//! 一个 1 MB 的常驻进程。自己不加载 dsh 的任何插件，只干四件事：
//!
//!   1. 收活（HTTP，127.0.0.1）
//!   2. 起一个 headless BOT 去干
//!   3. 等结果，返回
//!   4. 记 PID、管超时、清理跑飞的进程
//!
//! ## 为什么它不常驻 BOT，只常驻自己
//!
//! 常驻 BOT（TUI/web 形态）一个 300~400MB，五个就是 2GB。
//! BOT 真正干活的时间占比很低，常驻等于绝大部分时间在烧内存发呆。
//!
//! 所以：**常驻一个轻的管家，BOT 按需起停。**
//!
//! ## 这个文件只管什么
//!
//! **只做装配与路由**：读环境变量、建 state、挂路由、分派。
//! 具体业务在各自模块里：
//!
//! | 模块 | 管什么 |
//! | --- | --- |
//! | `error` | 错误类型、错误码、HTTP 状态映射 |
//! | `logging` | 结构化日志、级别、落文件 |
//! | `api` | 统一响应格式、接口自描述 |
//! | `team` | 群 目录、成员会话归属 |
//! | `bot` | 起停 BOT、解析事件流、超时 |
//!
//! ## 协议
//!
//!   GET  /          接口清单（自描述）
//!   GET  /health    健康检查
//!   GET  /groups     列 Team
//!   POST /groups     建 Team
//!   POST /run       让 BOT 干活
//!   GET  /bots      看 BOT 状态
//!   POST /stop      停 BOT

mod api;
mod bot;
mod error;
mod logging;
mod group;
mod job;

use std::collections::HashMap;
use std::env;
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::{json, Value};
use tiny_http::{Method, Server};

use error::DaemonError;
use logging::Level;
use group::GroupStore;

/// 进程内共享状态。**锁中毒不 panic**：用 `unwrap_or_else` 恢复。
pub struct State {
    /// key = "{groupId}:{bot}"，value = (pid, started)
    pub running: HashMap<String, (u32, Instant)>,
    /// 异步 job 表（D019）。同步调用不建 job。
    pub jobs: HashMap<String, job::Job>,
    pub started_at: Instant,
    pub agents_dir: PathBuf,
    pub groups: GroupStore,
    pub max_concurrent: usize,
    pub default_timeout_ms: u64,
}

pub type Shared = Arc<Mutex<State>>;

/// 拿锁，中毒也能继续（不让一次 panic 拖死整个 daemon）。
fn lock(state: &Shared) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 反查：这个 PID 是哪个 群 的哪个 BOT？
///
/// **这是「身份」的唯一权威来源。**
/// BOT 不需要知道自己的 groupId —— 它调进来时报 `process.pid`，
/// 我们查表就知道它是谁家的孩子。
///
/// 为什么可信：PID 是操作系统给的，只有真的被我们 spawn 出来的进程才会有条目。
fn identify_caller(state: &Shared, pid: u32) -> Option<(String, String)> {
    if pid == 0 {
        return None;
    }
    let s = lock(state);
    for (key, (run_pid, _)) in &s.running {
        if *run_pid == pid {
            if let Some((team, bot)) = key.split_once(':') {
                return Some((team.to_string(), bot.to_string()));
            }
        }
    }
    None
}

fn read_body(request: &mut tiny_http::Request) -> Value {
    let mut body = String::new();
    let _ = request.as_reader().take(1_000_000).read_to_string(&mut body);
    serde_json::from_str(&body).unwrap_or_else(|_| json!({}))
}

fn main() {
    let home = env::var("HOME").unwrap_or_else(|_| String::from("."));
    let dsh_home = env::var("DSH_HOME").unwrap_or_else(|_| format!("{}/.dsh", home));

    // 日志：级别可调，默认 INFO；文件落在 DSH_HOME/bot-daemon/daemon.log
    let log_level = Level::from_env();
    let log_file = PathBuf::from(format!("{}/bot-daemon/daemon.log", dsh_home));
    logging::init(log_level, Some(log_file));

    let port: u16 = env::var("DSH_BOT_DAEMON_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(18923);
    let max_concurrent: usize = env::var("DSH_BOT_MAX_CONCURRENT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4);
    let default_timeout_ms: u64 = env::var("DSH_BOT_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300_000);

    let agents_dir = PathBuf::from(format!("{}/agents", dsh_home));
    let groups_root = PathBuf::from(format!("{}/groups", dsh_home));

    let addr = format!("127.0.0.1:{}", port);
    let server = match Server::http(&addr) {
        Ok(s) => s,
        Err(e) => {
            log_error!("daemon.listen_failed", addr = &addr, error = e);
            std::process::exit(1);
        }
    };

    log_info!(
        "daemon.ready",
        addr = &addr,
        pid = std::process::id(),
        agents = &agents_dir.display(),
        groups = &groups_root.display(),
        max_concurrent = max_concurrent,
        timeout_ms = default_timeout_ms
    );

    let state: Shared = Arc::new(Mutex::new(State {
        running: HashMap::new(),
        jobs: HashMap::new(),
        started_at: Instant::now(),
        agents_dir,
        groups: GroupStore::new(groups_root),
        max_concurrent,
        default_timeout_ms,
    }));

    // ⚠️ 每个请求一个线程。
    //
    // 之前是 `for request in server.incoming_requests()` 串行处理 ——
    // /run 一次要等几十秒到几分钟，期间别的请求（/groups、/health）全被堵住。
    // 实测：苏见微调钟诗雨时嵌套两次推理，/groups 直接超时。
    //
    // 并发上限由 max_concurrent 控（/run 里检查），不是靠串行控。
    for request in server.incoming_requests() {
        let state = state.clone();
        std::thread::spawn(move || handle_request(state, request));
    }
}

fn handle_request(state: Shared, mut request: tiny_http::Request) {
    {
        let method = request.method().clone();
        let raw_url = request.url().to_string();
        let path = raw_url.split('?').next().unwrap_or("/").to_string();
        let query = raw_url
            .split_once('?')
            .map(|(_, q)| q.to_string())
            .unwrap_or_default();

        let started = Instant::now();

        match (&method, path.as_str()) {
            (Method::Get, "/") => api::index(request),

            (Method::Get, "/health") => {
                let s = lock(&state);
                api::ok(
                    request,
                    json!({
                        "daemonPid": std::process::id(),
                        "uptimeSec": s.started_at.elapsed().as_secs(),
                        "running": s.running.len(),
                        "maxConcurrent": s.max_concurrent,
                    }),
                );
            }

            // ---- 追加群史事件（供插件上报求援等）----
            (Method::Post, "/groups/log") => {
                let body = read_body(&mut request);
                let gid = body
                    .get("groupId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if gid.is_empty() {
                    api::fail(
                        request,
                        &DaemonError::InvalidRequest {
                            field: "groupId",
                            reason: "必填".into(),
                        },
                    );
                    return;
                }
                let mut event = body.get("event").cloned().unwrap_or(json!({}));
                if let Some(obj) = event.as_object_mut() {
                    obj.insert("at".into(), json!(group::now_stamp()));
                    obj.insert("group".into(), json!(gid));
                }
                let store = {
                    let s = lock(&state);
                    GroupStore::new(s.groups.root.clone())
                };
                if !store.exists(&gid) {
                    api::fail(
                        request,
                        &DaemonError::GroupNotFound {
                            group_id: gid.clone(),
                        },
                    );
                    return;
                }
                store.append_event(&gid, event);
                api::ok(request, json!({ "groupId": gid, "logged": true }));
            }

            // ---- 按会话找群 / 建群（D020：群由会话生）----
            (Method::Post, "/groups/resolve") => {
                let body = read_body(&mut request);
                let session = body
                    .get("originSession")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let origin = body
                    .get("origin")
                    .and_then(|v| v.as_str())
                    .unwrap_or("jiangwantang")
                    .to_string();
                let label = body.get("label").and_then(|v| v.as_str());

                let store = {
                    let s = lock(&state);
                    GroupStore::new(s.groups.root.clone())
                };
                // 先找：同一会话 resume 回来必须是同一个群
                if !session.is_empty() {
                    if let Some(gid) = store.find_by_session(&session) {
                        api::ok(request, json!({ "groupId": gid, "created": false }));
                        return;
                    }
                }
                match store.create_with_session(
                    label,
                    Some(&origin),
                    if session.is_empty() { None } else { Some(&session) },
                ) {
                    Ok(gid) => api::ok(request, json!({ "groupId": gid, "created": true })),
                    Err(e) => api::fail(request, &e),
                }
            }

            // ---- 异步 job 查询（D019）----
            (Method::Get, p) if p.starts_with("/jobs/") => {
                let id = p.trim_start_matches("/jobs/").to_string();
                let found = {
                    let s = lock(&state);
                    s.jobs.get(&id).map(|j| j.to_json())
                };
                match found {
                    Some(v) => api::ok(request, v),
                    None => api::fail(
                        request,
                        &DaemonError::InvalidRequest {
                            field: "jobId",
                            reason: format!("没有这个 job：{}", id),
                        },
                    ),
                }
            }

            (Method::Get, "/jobs") => {
                let group_filter = query
                    .split('&')
                    .find_map(|kv| kv.strip_prefix("groupId="))
                    .map(|s| s.to_string());
                let status_filter = query
                    .split('&')
                    .find_map(|kv| kv.strip_prefix("status="))
                    .map(|s| s.to_string());
                let jobs: Vec<Value> = {
                    let s = lock(&state);
                    let mut v: Vec<Value> = s
                        .jobs
                        .values()
                        .filter(|j| {
                            group_filter
                                .as_deref()
                                .map(|g| j.group_id.as_deref() == Some(g))
                                .unwrap_or(true)
                                && status_filter
                                    .as_deref()
                                    .map(|st| j.status.as_str() == st)
                                    .unwrap_or(true)
                        })
                        .map(|j| j.to_json())
                        .collect();
                    v.sort_by(|a, b| {
                        b.get("jobId")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .cmp(a.get("jobId").and_then(|v| v.as_str()).unwrap_or(""))
                    });
                    v
                };
                api::ok(request, json!({ "jobs": jobs, "count": jobs.len() }));
            }

            (Method::Get, "/groups") => {
                let working: Vec<(String, Option<String>)> = {
                    let s = lock(&state);
                    s.running
                        .values()
                        .map(|_| (String::new(), None))
                        .collect()
                };
                let _ = working; // 占位：working 明细在下面按 groupId 查
                let snapshot = {
                    let s = lock(&state);
                    let mut map: HashMap<String, Vec<Value>> = HashMap::new();
                    for (key, (pid, _)) in &s.running {
                        if let Some((team, bot)) = key.split_once(':') {
                            map.entry(team.to_string()).or_default().push(json!({
                                "name": bot,
                                "pid": pid,
                            }));
                        }
                    }
                    map
                };
                let payload = {
                    let s = lock(&state);
                    let include_archived = query.contains("includeArchived=true")
                        || query.contains("includeArchived=1");
                    s.groups.list(
                        |group_id| snapshot.get(group_id).cloned().unwrap_or_default(),
                        include_archived,
                    )
                };
                api::ok(request, payload);
            }

            (Method::Post, "/groups") => {
                let body = read_body(&mut request);
                let label = body.get("label").and_then(|v| v.as_str());
                // 发起者：谁建的 群 谁就是 captain（D010 / D025）
                let origin = body.get("origin").and_then(|v| v.as_str());
                let result = {
                    let s = lock(&state);
                    s.groups.create(label, origin)
                };
                match result {
                    Ok(group_id) => api::ok(
                        request,
                        json!({ "groupId": group_id, "createdAt": group::now_stamp() }),
                    ),
                    Err(e) => api::fail(request, &e),
                }
            }

            (Method::Post, "/groups/archive") | (Method::Post, "/groups/unarchive") => {
                let body = read_body(&mut request);
                let group_id = body
                    .get("groupId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if group_id.is_empty() {
                    api::fail(
                        request,
                        &DaemonError::InvalidRequest {
                            field: "groupId",
                            reason: "必填".into(),
                        },
                    );
                    return;
                }
                let is_archive = path.ends_with("/archive");
                let result = {
                    let s = lock(&state);
                    if is_archive {
                        s.groups.archive(&group_id)
                    } else {
                        s.groups.unarchive(&group_id)
                    }
                };
                match result {
                    Ok(()) => api::ok(
                        request,
                        json!({ "groupId": group_id, "archived": is_archive }),
                    ),
                    Err(e) => api::fail(request, &e),
                }
            }

            (Method::Post, "/groups/pause") | (Method::Post, "/groups/resume") => {
                let body = read_body(&mut request);
                let group_id = body
                    .get("groupId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if group_id.is_empty() {
                    api::fail(
                        request,
                        &DaemonError::InvalidRequest {
                            field: "groupId",
                            reason: "必填".into(),
                        },
                    );
                    return;
                }
                let is_pause = path.ends_with("/pause");

                // 暂停时先把该 群 正在跑的进程杀掉（D001：序列化之后杀）
                let mut killed = 0u32;
                if is_pause {
                    let pids: Vec<u32> = {
                        let s = lock(&state);
                        s.running
                            .iter()
                            .filter(|(k, _)| k.starts_with(&format!("{}:", group_id)))
                            .filter_map(|(_, (pid, _))| if *pid > 0 { Some(*pid) } else { None })
                            .collect()
                    };
                    for pid in pids {
                        if std::process::Command::new("kill")
                            .arg(pid.to_string())
                            .status()
                            .is_ok()
                        {
                            killed += 1;
                        }
                        log_warn!("team.pause_killed", team = &group_id, pid = pid);
                    }
                }

                let result = {
                    let s = lock(&state);
                    if is_pause {
                        s.groups.pause(&group_id)
                    } else {
                        s.groups.resume(&group_id)
                    }
                };
                match result {
                    Ok(mut data) => {
                        if is_pause && killed > 0 {
                            data["killedProcesses"] = json!(killed);
                        }
                        api::ok(request, data);
                    }
                    Err(e) => api::fail(request, &e),
                }
            }

            (Method::Post, "/run") => {
                let body = read_body(&mut request);
                let name = body
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let message = body
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();

                if name.is_empty() {
                    api::fail(
                        request,
                        &DaemonError::InvalidRequest {
                            field: "name",
                            reason: "必填，且不能为空".into(),
                        },
                    );
                    return;
                }
                if message.is_empty() {
                    api::fail(
                        request,
                        &DaemonError::InvalidRequest {
                            field: "message",
                            reason: "必填，且不能为空".into(),
                        },
                    );
                    return;
                }

                // ---- 身份：优先用 PID 反查，其次用显式 groupId ----
                //
                // callerPid 在 running 表里 → 它是被我们 spawn 的某个 BOT，
                //   那么它属于哪个 Team、叫什么名字，我们说了算。
                // 不在表里（或没传）→ 它是外部调用者（主会话 / curl），
                //   用 body 里的 groupId。
                let caller_pid = body
                    .get("callerPid")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;
                let identified = identify_caller(&state, caller_pid);

                let (group_id, sender) = match identified {
                    Some((team, bot)) => {
                        log_debug!("caller.identified", pid = caller_pid, team = &team, bot = &bot);
                        (Some(team), Some(bot))
                    }
                    None => (
                        body.get("groupId")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                        // 外部调用者可以自报名字（主会话就是 jiangwantang）
                        body.get("sender")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                    ),
                };

                // 群 存在性校验：给了 groupId 就必须真的存在
                if let Some(tid) = &group_id {
                    let exists = {
                        let s = lock(&state);
                        s.groups.exists(tid)
                    };
                    if !exists {
                        api::fail(
                            request,
                            &DaemonError::GroupNotFound {
                                group_id: tid.clone(),
                            },
                        );
                        return;
                    }
                    // 暂停的 群 拒绝派活（D001 的「暂停」在这里生效）
                    let paused = {
                        let s = lock(&state);
                        s.groups.is_paused(tid)
                    };
                    if paused {
                        api::fail(
                            request,
                            &DaemonError::GroupPaused {
                                group_id: tid.clone(),
                            },
                        );
                        return;
                    }
                }

                // D023：不设并发上限。槽位不拦、不排队，让 OS 管内存。
                // 注意 `running` 表本身仍然维护——它是 PID 反查的唯一依据（D013/D019）。
                let (agents_dir, timeout_default) = {
                    let s = lock(&state);
                    (s.agents_dir.clone(), s.default_timeout_ms)
                };

                let timeout_ms = body
                    .get("timeoutMs")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(timeout_default);

                let is_async = body
                    .get("async")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                // D024：父结束时子怎么办。默认 terminate（不放任）。
                let on_parent_close = body
                    .get("onParentClose")
                    .and_then(|v| v.as_str())
                    .unwrap_or("terminate")
                    .to_string();

                // 登记为运行中
                let key = format!("{}:{}", group_id.as_deref().unwrap_or("default"), name);
                let job_id = job::new_job_id();
                {
                    let mut s = lock(&state);
                    s.running.insert(key.clone(), (0, Instant::now()));
                    if is_async {
                        let job = job::Job::new(
                            job_id.clone(),
                            name.clone(),
                            group_id.clone(),
                            None,                 // parent_job 留空：PID 反查父 job 待补
                            sender.clone(),
                            on_parent_close.clone(),
                        );
                        s.jobs.insert(job_id.clone(), job);
                    }
                }

                // ---- 异步分支：起线程跑，立刻返回 jobId（D019）----
                if is_async {
                    let state_bg = state.clone();
                    let key_bg = key.clone();
                    let job_bg = job_id.clone();
                    let groups_bg = {
                        let s = lock(&state);
                        GroupStore::new(s.groups.root.clone())
                    };
                    let agents_bg = agents_dir.clone();
                    let name_bg = name.clone();
                    let message_bg = message.clone();
                    let gid_bg = group_id.clone();
                    let sender_bg = sender.clone();
                    std::thread::spawn(move || {
                        let state_for_pid = state_bg.clone();
                        let key_for_pid = key_bg.clone();
                        let job_for_pid = job_bg.clone();
                        let result = bot::run(
                            &groups_bg,
                            &agents_bg,
                            &name_bg,
                            &message_bg,
                            gid_bg.as_deref(),
                            sender_bg.as_deref(),
                            timeout_ms,
                            &move |real_pid: u32| {
                                if let Ok(mut s) = state_for_pid.lock() {
                                    if let Some(entry) = s.running.get_mut(&key_for_pid) {
                                        entry.0 = real_pid;
                                    }
                                    if let Some(j) = s.jobs.get_mut(&job_for_pid) {
                                        j.pid = real_pid;
                                    }
                                }
                            },
                        );
                        let (status, reply, err, mailbox) = match &result {
                            Ok(o) => {
                                let mb = groups_bg.write_mailbox(
                                    gid_bg.as_deref(),
                                    &job_bg,
                                    &o.bot,
                                    &o.reply,
                                    sender_bg.as_deref(),
                                );
                                (job::JobStatus::Done, Some(o.reply.clone()), None::<String>, mb)
                            }
                            Err(e) => (job::JobStatus::Failed, None, Some(format!("{e}")), None),
                        };
                        {
                            if let Ok(mut s) = state_bg.lock() {
                                s.running.remove(&key_bg);
                                if let Some(j) = s.jobs.get_mut(&job_bg) {
                                    j.status = status;
                                    j.reply = reply;
                                    j.error = err;
                                    j.finished = Some(Instant::now());
                                    j.mailbox = mailbox.clone();
                                }
                            }
                        }
                        if let Some(gid) = &gid_bg {
                            groups_bg.append_event(
                                gid,
                                serde_json::json!({
                                    "at": group::now_stamp(),
                                    "event": if status == job::JobStatus::Done { "task.returned" } else { "task.failed" },
                                    "group": gid,
                                    "job": job_bg,
                                    "bot": name_bg,
                                    "mailbox": mailbox,
                                }),
                            );
                        }
                    });
                    api::ok(
                        request,
                        json!({
                            "jobId": job_id,
                            "status": "running",
                            "bot": name,
                            "groupId": group_id,
                            "onParentClose": on_parent_close,
                        }),
                    );
                    return;
                }

                let groups = {
                    let s = lock(&state);
                    GroupStore::new(s.groups.root.clone())
                };

                // D028 派活即入群：**在 spawn 之前**真写进 meta.json。
                // 晚一步的话，BOT 干活时 group_whoami 看不见自己。
                if let Some(gid) = &group_id {
                    if groups.ensure_member(gid, &name, "worker") {
                        groups.append_event(
                            gid,
                            json!({
                                "at": group::now_stamp(),
                                "event": "member.joined",
                                "group": gid,
                                "bot": name,
                                "why": "dispatched",
                            }),
                        );
                    }
                    groups.append_event(
                        gid,
                        json!({
                            "at": group::now_stamp(),
                            "event": "task.dispatched",
                            "group": gid,
                            "job": if is_async { json!(job_id) } else { json!(null) },
                            "bot": name,
                            "from": sender,
                        }),
                    );
                }

                let state_for_pid = state.clone();
                let key_for_pid = key.clone();
                let result = bot::run(
                    &groups,
                    &agents_dir,
                    &name,
                    &message,
                    group_id.as_deref(),
                    sender.as_deref(),
                    timeout_ms,
                    &move |real_pid: u32| {
                        // 回填真实 PID —— 这是 PID 反查的唯一依据
                        if let Ok(mut s) = state_for_pid.lock() {
                            if let Some(entry) = s.running.get_mut(&key_for_pid) {
                                entry.0 = real_pid;
                            }
                        }
                    },
                );

                // 清登记
                {
                    let mut s = lock(&state);
                    s.running.remove(&key);
                }

                match result {
                    Ok(outcome) => {
                        let mut data = json!({
                            "reply": outcome.reply,
                            "sessionId": outcome.session_id,
                            "pid": outcome.pid,
                            "ms": outcome.ms,
                            "continued": outcome.continued,
                        });
                        if let Some(tid) = &group_id {
                            data["groupId"] = json!(tid);
                        }
                        if let Some(from) = &sender {
                            data["from"] = json!(from);
                        }
                        api::ok(request, data);
                    }
                    Err(e) => api::fail(request, &e),
                }
            }

            (Method::Get, "/bots") => {
                let team_filter = query
                    .split('&')
                    .find_map(|kv| kv.strip_prefix("groupId="))
                    .map(|s| s.to_string());
                let (agents_dir, running) = {
                    let s = lock(&state);
                    let running: Vec<(String, u32, Option<String>)> = s
                        .running
                        .iter()
                        .filter_map(|(key, (pid, _))| {
                            let (team, bot) = key.split_once(':')?;
                            Some((bot.to_string(), *pid, Some(team.to_string())))
                        })
                        .collect();
                    (s.agents_dir.clone(), running)
                };
                let payload = bot::snapshot(&agents_dir, team_filter.as_deref(), &running);
                api::ok(request, payload);
            }

            (Method::Post, "/stop") => {
                let body = read_body(&mut request);
                let only = body.get("name").and_then(|v| v.as_str());
                let mut stopped = 0u32;
                {
                    let mut s = lock(&state);
                    let keys: Vec<String> = s
                        .running
                        .keys()
                        .filter(|k| {
                            only.map(|n| k.ends_with(&format!(":{}", n))).unwrap_or(true)
                        })
                        .cloned()
                        .collect();
                    for k in keys {
                        if let Some((pid, _)) = s.running.get(&k) {
                            if *pid > 0 {
                                let _ = std::process::Command::new("kill")
                                    .arg(pid.to_string())
                                    .status();
                                stopped += 1;
                            }
                        }
                        s.running.remove(&k);
                    }
                }
                log_warn!("bot.stopped_by_request", count = stopped);
                api::ok(request, json!({ "stopped": stopped }));
            }

            _ => api::fail_plain(
                request,
                404,
                "NOT_FOUND",
                &format!("没有这个路由：{} {}", method, path),
            ),
        }

        log_debug!(
            "http.request",
            method = &method,
            path = &path,
            ms = started.elapsed().as_millis()
        );
    }
}

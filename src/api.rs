//! API 层：统一响应格式、错误映射、自描述。
//!
//! ## 为什么要有这一层
//!
//! 现在 `main.rs` 里到处 `json!({ "ok": true, ... })`，字段随手加。
//! 调用方（bot_send 插件、curl 脚本、将来的 Web UI）只能靠猜。
//!
//! 这一层做三件事：
//!
//!   1. **统一成功/失败的形状** —— 失败永远是 `{ok:false, error:{code,message,details}}`
//!   2. **错误 → HTTP 状态** 的映射只在这里写一次
//!   3. **自描述** —— `GET /` 返回接口清单，不看文档也能用
//!
//! ## 响应契约
//!
//! 成功：
//! ```json
//! { "ok": true, "data": { ... } }
//! ```
//!
//! 失败：
//! ```json
//! {
//!   "ok": false,
//!   "error": {
//!     "code": "BOT_NOT_FOUND",
//!     "message": "没有这个 BOT：xxx（现有：a, b）",
//!     "details": { "name": "xxx", "available": ["a", "b"] }
//!   }
//! }
//! ```
//!
//! **调用方按 `code` 分支，不要按 `message` 分支**（message 会改字）。

use serde_json::{json, Value};
use tiny_http::{Header, Request, Response};

use crate::error::DaemonError;

const API_VERSION: &str = "1";

fn json_header() -> Header {
    Header::from_bytes(&b"Content-Type"[..], &b"application/json; charset=utf-8"[..])
        .expect("static header")
}

fn respond_raw(request: Request, code: u16, payload: Value) {
    let body = payload.to_string();
    let response = Response::from_string(body)
        .with_status_code(code)
        .with_header(json_header());
    let _ = request.respond(response);
}

/// 成功响应。data 放业务字段。
pub fn ok(request: Request, data: Value) {
    respond_raw(request, 200, json!({ "ok": true, "data": data, "apiVersion": API_VERSION }));
}

/// 失败响应。状态码与 code 由 `DaemonError` 自己决定。
pub fn fail(request: Request, err: &DaemonError) {
    let mut payload = err.to_json();
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("apiVersion".into(), json!(API_VERSION));
    }
    respond_raw(request, err.http_status(), payload);
}

/// 不是 `DaemonError` 的兜底失败（比如路由不存在）。
pub fn fail_plain(request: Request, status: u16, code: &str, message: &str) {
    respond_raw(
        request,
        status,
        json!({
            "ok": false,
            "error": { "code": code, "message": message, "details": {} },
            "apiVersion": API_VERSION,
        }),
    );
}

/// `GET /` —— 接口清单。**让 API 自己说明自己。**
pub fn index(request: Request) {
    let payload = json!({
        "ok": true,
        "apiVersion": API_VERSION,
        "service": "dsh-bot-daemon",
        "description": "BOT 进程池的看门人。常驻一个轻进程，BOT 按需起停。",
        "endpoints": [
            {
                "method": "GET", "path": "/",
                "desc": "这份清单"
            },
            {
                "method": "GET", "path": "/health",
                "desc": "健康检查：daemon 活了多久、几个任务在跑",
                "response": { "daemonPid": "number", "uptimeSec": "number", "running": "number", "maxConcurrent": "number" }
            },
            {
                "method": "POST", "path": "/groups",
                "desc": "建一个群，返回 groupId",
                "body": { "label?": "string —— 人类可读的名字，比如「绘画任务」" },
                "response": { "groupId": "string", "createdAt": "string" }
            },
            {
                "method": "GET", "path": "/groups",
                "desc": "列出所有群，含成员、会话与正在工作的进程"
            },
            {
                "method": "POST", "path": "/run",
                "desc": "让一个 BOT 干活。默认等它答完（同步）；传 async:true 立刻返回 jobId（D019）。同 groupId 下会话连续，跨 groupId 完全隔离。",
                "body": {
                    "name": "string —— BOT 名（目录名或中文名，如 zhongshiyu / 钟诗雨）",
                    "message": "string",
                    "groupId?": "string —— 不传就走全局会话（会串台，仅调试用）",
                    "timeoutMs?": "number",
                    "async?": "boolean —— true 时不等待，返回 jobId",
                    "onParentClose?": "string —— terminate(默认) / abandon / request_cancel（D024）"
                },
                "response": {
                    "sync": { "reply": "string", "sessionId": "string", "pid": "number", "ms": "number" },
                    "async": { "jobId": "string", "status": "running", "bot": "string", "groupId": "string" }
                }
            },
            {
                "method": "GET", "path": "/jobs/:jobId",
                "desc": "查一个异步 job 的状态与结果（D019）",
                "response": { "jobId": "string", "status": "running|done|failed|orphan", "reply": "string", "mailbox": "string", "pid": "number", "ms": "number" }
            },
            {
                "method": "GET", "path": "/jobs?groupId=xxx&status=done",
                "desc": "列 job。可按群、按状态过滤——孤儿 job 用 status=orphan 找"
            },
            {
                "method": "GET", "path": "/bots?groupId=xxx",
                "desc": "看各 BOT 状态。带 groupId 只看这个群。"
            },
            {
                "method": "POST", "path": "/stop",
                "desc": "停掉正在跑的 BOT 进程（保留会话）",
                "body": { "name?": "string —— 不传就停全部" }
            }
        ],
        "errors": {
            "BOT_NOT_FOUND": { "http": 404, "desc": "没有这个 BOT（details.available 给可用名单；中文名会自动解析）" },
            "GROUP_NOT_FOUND": { "http": 404, "desc": "没有这个群" },
            "INVALID_REQUEST": { "http": 400, "desc": "参数缺失或非法" },
            "BOT_TIMEOUT": { "http": 504, "desc": "BOT 超时" },
            "BOT_NO_ANSWER": { "http": 502, "desc": "BOT 退出但没给最终答案" },
            "SPAWN_FAILED": { "http": 500, "desc": "进程起不来" },
            "IO_FAILED": { "http": 500, "desc": "磁盘操作失败" },
            "INTERNAL": { "http": 500, "desc": "内部错误" }
        }
    });
    respond_raw(request, 200, payload);
}

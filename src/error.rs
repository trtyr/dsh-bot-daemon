//! 错误系统。
//!
//! ## 为什么不用 `String`
//!
//! `format!("拉不起来：{}", e)` 这种错误有三个毛病：
//!
//!   1. **不可判定** —— 调用方只能拿字符串做 contains，一改字就崩
//!   2. **没有分类** —— 「BOT 不存在」和「进程起不来」是两种完全不同的失败，
//!      前者是调用方的问题（改参数），后者是环境的问题（重试或修机器）
//!   3. **没有错误码** —— API 调用方没法写 if 分支
//!
//! 所以：**每个错误有稳定的 code，有结构化的字段，有明确的 HTTP 状态。**
//!
//! ## 错误码约定
//!
//! 大写下划线，永不复用，改名等于破坏兼容：
//!
//! | code | HTTP | 含义 | 谁的问题 |
//! | --- | --- | --- | --- |
//! | `BOT_NOT_FOUND` | 404 | 没有这个 BOT | 调用方（名字错了/人设没建） |
//! | `GROUP_NOT_FOUND` | 404 | 没有这个群 | 调用方 |
//! | `GROUP_PAUSED` | 409 | 群 已暂停，先 resume | 调用方 |
//! | `INVALID_REQUEST` | 400 | 参数缺失或非法 | 调用方 |
//! | ~~`CONCURRENT_LIMIT`~~ | — | **已废（D023：不设并发上限）** | — |
//! | `BOT_TIMEOUT` | 504 | BOT 超时没答完 | 环境/模型 |
//! | `BOT_NO_ANSWER` | 502 | BOT 退出但没给最终答案 | BOT 内部 |
//! | `SPAWN_FAILED` | 500 | 进程起不来 | 环境（dsh 没装/权限） |
//! | `IO_FAILED` | 500 | 读写磁盘失败 | 环境 |
//! | `INTERNAL` | 500 | 其他内部错误 | 我们自己的 bug |

use std::fmt;

use serde_json::{json, Value};

#[derive(Debug)]
pub enum DaemonError {
    BotNotFound { name: String, available: Vec<String> },
    JobNotFound { job_id: String },
    GroupNotFound { group_id: String },
    GroupPaused { group_id: String },
    InvalidRequest { field: &'static str, reason: String },
    ConcurrentLimit { max: usize, current: usize },
    BotTimeout { name: String, timeout_ms: u64 },
    BotNoAnswer { name: String, exit_code: Option<i32>, stderr_tail: String },
    SpawnFailed { name: String, reason: String },
    IoFailed { context: &'static str, reason: String },
    Internal { context: &'static str, reason: String },
}

impl DaemonError {
    /// 稳定的错误码。**调用方按这个分支，不要按 message 分支。**
    pub fn code(&self) -> &'static str {
        match self {
            DaemonError::BotNotFound { .. } => "BOT_NOT_FOUND",
            DaemonError::JobNotFound { .. } => "JOB_NOT_FOUND",
            DaemonError::GroupNotFound { .. } => "GROUP_NOT_FOUND",
            DaemonError::GroupPaused { .. } => "GROUP_PAUSED",
            DaemonError::InvalidRequest { .. } => "INVALID_REQUEST",
            DaemonError::ConcurrentLimit { .. } => "CONCURRENT_LIMIT",
            DaemonError::BotTimeout { .. } => "BOT_TIMEOUT",
            DaemonError::BotNoAnswer { .. } => "BOT_NO_ANSWER",
            DaemonError::SpawnFailed { .. } => "SPAWN_FAILED",
            DaemonError::IoFailed { .. } => "IO_FAILED",
            DaemonError::Internal { .. } => "INTERNAL",
        }
    }

    /// 对应的 HTTP 状态码。
    pub fn http_status(&self) -> u16 {
        match self {
            DaemonError::BotNotFound { .. } => 404,
            DaemonError::JobNotFound { .. } => 404,
            DaemonError::GroupNotFound { .. } => 404,
            DaemonError::GroupPaused { .. } => 409,
            DaemonError::InvalidRequest { .. } => 400,
            DaemonError::ConcurrentLimit { .. } => 429,
            DaemonError::BotTimeout { .. } => 504,
            DaemonError::BotNoAnswer { .. } => 502,
            DaemonError::SpawnFailed { .. } => 500,
            DaemonError::IoFailed { .. } => 500,
            DaemonError::Internal { .. } => 500,
        }
    }

    /// 给调用方看的一句话（中文，能直接展示给人）。
    pub fn message(&self) -> String {
        match self {
            DaemonError::BotNotFound { name, available } => {
                if available.is_empty() {
                    format!("没有这个 BOT：{}", name)
                } else {
                    format!("没有这个 BOT：{}（现有：{}）", name, available.join(", "))
                }
            }
            DaemonError::GroupNotFound { group_id } => format!("没有这个群：{}", group_id),
            DaemonError::JobNotFound { job_id } => format!("没有这个 job：{}", job_id),
            DaemonError::GroupPaused { group_id } => {
                format!("群 `{}` 现在是暂停状态，先 resume 再来派活", group_id)
            }
            DaemonError::InvalidRequest { field, reason } => {
                format!("参数 `{}` 不合法：{}", field, reason)
            }
            DaemonError::ConcurrentLimit { max, current } => {
                format!("并发已满（{} 个在跑，上限 {}），等一会儿再试", current, max)
            }
            DaemonError::BotTimeout { name, timeout_ms } => {
                format!("BOT `{}` 超过 {} ms 还没答完，已中止", name, timeout_ms)
            }
            DaemonError::BotNoAnswer { name, exit_code, stderr_tail } => {
                let tail = if stderr_tail.is_empty() {
                    String::new()
                } else {
                    format!("；stderr 尾部：{}", stderr_tail)
                };
                format!("BOT `{}` 没有给出最终答案（退出码 {:?}）{}", name, exit_code, tail)
            }
            DaemonError::SpawnFailed { name, reason } => {
                format!("拉不起 BOT `{}`：{}", name, reason)
            }
            DaemonError::IoFailed { context, reason } => {
                format!("磁盘操作失败（{}）：{}", context, reason)
            }
            DaemonError::Internal { context, reason } => {
                format!("内部错误（{}）：{}", context, reason)
            }
        }
    }

    /// 结构化字段：调用方可以程序化读取，不用解析 message。
    pub fn details(&self) -> Value {
        match self {
            DaemonError::BotNotFound { name, available } => {
                json!({ "name": name, "available": available })
            }
            DaemonError::GroupNotFound { group_id } => json!({ "groupId": group_id }),
            DaemonError::JobNotFound { job_id } => json!({ "jobId": job_id }),
            DaemonError::GroupPaused { group_id } => json!({ "groupId": group_id }),
            DaemonError::InvalidRequest { field, reason } => {
                json!({ "field": field, "reason": reason })
            }
            DaemonError::ConcurrentLimit { max, current } => {
                json!({ "max": max, "current": current })
            }
            DaemonError::BotTimeout { name, timeout_ms } => {
                json!({ "name": name, "timeoutMs": timeout_ms })
            }
            DaemonError::BotNoAnswer { name, exit_code, stderr_tail } => json!({
                "name": name,
                "exitCode": exit_code,
                "stderrTail": stderr_tail,
            }),
            DaemonError::SpawnFailed { name, reason } => json!({ "name": name, "reason": reason }),
            DaemonError::IoFailed { context, reason } => {
                json!({ "context": context, "reason": reason })
            }
            DaemonError::Internal { context, reason } => {
                json!({ "context": context, "reason": reason })
            }
        }
    }

    /// 转成统一的错误响应体。
    pub fn to_json(&self) -> Value {
        json!({
            "ok": false,
            "error": {
                "code": self.code(),
                "message": self.message(),
                "details": self.details(),
            }
        })
    }
}

impl fmt::Display for DaemonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.code(), self.message())
    }
}

impl std::error::Error for DaemonError {}

/// 把 io::Error 连同「在干什么」一起包起来 —— 光有 `No such file` 没法排障。
pub fn io_err(context: &'static str, e: std::io::Error) -> DaemonError {
    DaemonError::IoFailed { context, reason: e.to_string() }
}

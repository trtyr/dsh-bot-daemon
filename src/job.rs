//! Job：一次异步派活的句柄。
//!
//! ## 为什么需要它
//!
//! `bot_dispatch` 是异步的——派完立刻返回 `jobId`，调用方继续干自己的事。
//! 但 BOT 是进程，跑完就退出：**结果必须落在没人也会活的地方**。
//!
//! 所以每个 job 的结果双写：
//!
//! 1. `State.jobs` 内存表（调用方等在那儿时直接拿）
//! 2. `groups/<id>/workspace/mailbox/<jobId>-<bot>.md`（进程死了、daemon 重启了都还在）
//!
//! ## 状态机
//!
//! ```text
//! running ──► done
//!    │
//!    ├──► failed
//!    └──► orphan（声明 abandon 但一直没人取；见 D024）
//! ```

use std::time::Instant;

use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Running,
    Done,
    Failed,
    Orphan,
}

impl JobStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            JobStatus::Running => "running",
            JobStatus::Done => "done",
            JobStatus::Failed => "failed",
            JobStatus::Orphan => "orphan",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Job {
    pub id: String,
    pub bot: String,
    pub group_id: Option<String>,
    /// 上层 job：BOT 派活时靠 PID 反查得到（D019）。
    pub parent_job: Option<String>,
    /// 谁派的（sender 或 PID 反查出的 bot 名）。
    pub spawned_by: Option<String>,
    pub status: JobStatus,
    pub reply: Option<String>,
    pub error: Option<String>,
    pub pid: u32,
    pub started: Instant,
    pub finished: Option<Instant>,
    /// 父结束时的处置（D024）。默认 terminate。
    pub on_parent_close: String,
    /// 结果落盘路径（相对群目录）。
    pub mailbox: Option<String>,
}

impl Job {
    pub fn new(
        id: String,
        bot: String,
        group_id: Option<String>,
        parent_job: Option<String>,
        spawned_by: Option<String>,
        on_parent_close: String,
    ) -> Self {
        Self {
            id,
            bot,
            group_id,
            parent_job,
            spawned_by,
            status: JobStatus::Running,
            reply: None,
            error: None,
            pid: 0,
            started: Instant::now(),
            finished: None,
            on_parent_close,
            mailbox: None,
        }
    }

    pub fn to_json(&self) -> Value {
        let ms = self
            .finished
            .map(|f| f.duration_since(self.started).as_millis() as u64)
            .unwrap_or_else(|| self.started.elapsed().as_millis() as u64);
        json!({
            "jobId": self.id,
            "bot": self.bot,
            "groupId": self.group_id,
            "parentJob": self.parent_job,
            "spawnedBy": self.spawned_by,
            "status": self.status.as_str(),
            "reply": self.reply,
            "error": self.error,
            "pid": self.pid,
            "ms": ms,
            "onParentClose": self.on_parent_close,
            "mailbox": self.mailbox,
        })
    }
}

/// 生成 jobId：`job-<8 位 hex>`。时间戳够唯一（同进程内加一个计数更稳，
/// 但 daemon 里同一毫秒几乎不可能派两个活）。
pub fn new_job_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("job-{:08x}", (nanos as u64) & 0xffff_ffff)
}

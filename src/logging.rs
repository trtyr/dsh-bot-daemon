//! 日志系统。
//!
//! ## 为什么自己写而不是引 `tracing`
//!
//! 这个 daemon 的目标是「极小的常驻进程」。`tracing` + `tracing-subscriber`
//! 会带来几十个依赖、几 MB 二进制。而我们只需要：
//!
//!   · 五个级别
//!   · 结构化字段（键值对，不是拼字符串）
//!   · 同时写 stdout 和文件
//!   · 按天切、能轮转
//!
//! 这就够了。
//!
//! ## 级别约定
//!
//! | 级别 | 什么时候用 | 例子 |
//! | --- | --- | --- |
//! | ERROR | 这一件事失败了，且不是调用方的错 | 磁盘写不进去、BOT 启动失败 |
//! | WARN | 能继续，但值得看一眼 | 并发已满拒绝了请求、BOT 超时被杀 |
//! | INFO | 状态变化 | 群 建了、BOT 起停、请求处理完 |
//! | DEBUG | 排障细节 | 解析到的事件、session 变更 |
//! | TRACE | 逐行追踪 | 原始 JSON 行 |
//!
//! **关键路径必须有日志**：请求进来、BOT 起、BOT 停、请求结束。
//! 没有这些，线上出事只能瞎猜。
//!
//! ## 格式
//!
//! 一行一条，固定的「时间 级别 事件 字段…」顺序，人看得懂，机器也能 grep：
//!
//! ```text
//! 2026-11-03T10:35:12Z INFO  bot.start      bot=sujianwei pid=93200 group=G-413ce528
//! 2026-11-03T10:35:17Z INFO  bot.finish     bot=sujianwei ok=true ms=5132
//! 2026-11-03T10:35:17Z WARN  bot.timeout    bot=zhongshiyu timeout_ms=300000
//! ```

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    pub fn as_str(&self) -> &'static str {
        match self {
            Level::Trace => "TRACE",
            Level::Debug => "DEBUG",
            Level::Info => "INFO ",
            Level::Warn => "WARN ",
            Level::Error => "ERROR",
        }
    }

    pub fn from_env() -> Level {
        match std::env::var("DSH_BOT_LOG_LEVEL").ok().as_deref() {
            Some("trace") => Level::Trace,
            Some("debug") => Level::Debug,
            Some("warn") => Level::Warn,
            Some("error") => Level::Error,
            _ => Level::Info,
        }
    }
}

pub struct Logger {
    level: Level,
    file: Option<Mutex<std::fs::File>>,
}

static LOGGER: Mutex<Option<&'static Logger>> = Mutex::new(None);

/// 初始化全局 logger。重复调用无副作用（只生效第一次）。
pub fn init(level: Level, file_path: Option<PathBuf>) -> &'static Logger {
    let file = file_path.and_then(|p| {
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        OpenOptions::new().create(true).append(true).open(p).ok()
    });
    let logger = Box::leak(Box::new(Logger {
        level,
        file: file.map(Mutex::new),
    }));
    if let Ok(mut slot) = LOGGER.lock() {
        if slot.is_none() {
            *slot = Some(logger);
        }
    }
    logger
}

fn global() -> Option<&'static Logger> {
    LOGGER.lock().ok().and_then(|s| *s)
}

fn timestamp() -> String {
    use std::time::Duration;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    let secs = now.as_secs();
    // 简易 UTC 分解（不引 chrono，够用）
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // 从 1970-01-01 起算日期
    let mut year = 1970i64;
    let mut d = days as i64;
    loop {
        let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
        let yd = if leap { 366 } else { 365 };
        if d < yd {
            break;
        }
        d -= yd;
        year += 1;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let months = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut month = 0usize;
    let mut dd = d;
    while month < 12 && dd >= months[month] as i64 {
        dd -= months[month] as i64;
        month += 1;
    }
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year,
        month + 1,
        dd + 1,
        h,
        m,
        s
    )
}

impl Logger {
    pub fn log(&self, level: Level, event: &str, fields: &[(&str, String)]) {
        if level < self.level {
            return;
        }
        let mut line = format!("{} {} {:<16}", timestamp(), level.as_str(), event);
        for (k, v) in fields {
            line.push_str(&format!(" {}={}", k, quote_if_needed(v)));
        }
        line.push('\n');

        // stdout（容器/前台能看到）
        print!("{}", line);

        // 文件（事后查）
        if let Some(file) = &self.file {
            if let Ok(mut f) = file.lock() {
                let _ = f.write_all(line.as_bytes());
                let _ = f.flush();
            }
        }
    }
}

/// 值里有空格就加引号，方便 grep 与肉眼扫描。
fn quote_if_needed(v: &str) -> String {
    if v.contains(' ') || v.contains('\t') || v.is_empty() {
        format!("{:?}", v)
    } else {
        v.to_string()
    }
}

#[macro_export]
macro_rules! log_error {
    ($event:expr $(, $k:ident = $v:expr)* $(,)?) => {
        if let Some(l) = $crate::logging::global_ref() {
            l.log($crate::logging::Level::Error, $event, &[$( (stringify!($k), format!("{}", $v)) ),*]);
        }
    };
}

#[macro_export]
macro_rules! log_warn {
    ($event:expr $(, $k:ident = $v:expr)* $(,)?) => {
        if let Some(l) = $crate::logging::global_ref() {
            l.log($crate::logging::Level::Warn, $event, &[$( (stringify!($k), format!("{}", $v)) ),*]);
        }
    };
}

#[macro_export]
macro_rules! log_info {
    ($event:expr $(, $k:ident = $v:expr)* $(,)?) => {
        if let Some(l) = $crate::logging::global_ref() {
            l.log($crate::logging::Level::Info, $event, &[$( (stringify!($k), format!("{}", $v)) ),*]);
        }
    };
}

#[macro_export]
macro_rules! log_debug {
    ($event:expr $(, $k:ident = $v:expr)* $(,)?) => {
        if let Some(l) = $crate::logging::global_ref() {
            l.log($crate::logging::Level::Debug, $event, &[$( (stringify!($k), format!("{}", $v)) ),*]);
        }
    };
}

/// 给宏用的公开访问点。
pub fn global_ref() -> Option<&'static Logger> {
    global()
}

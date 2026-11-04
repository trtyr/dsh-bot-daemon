//! Team：磁盘上的目录 + meta，以及会话的归属。
//!
//! ## 一个 Team 是什么
//!
//! 不是一个进程，是**一份命名空间**：
//!
//! ```text
//! ~/.dsh/teams/T-413ce528/
//! ├── meta.json
//! └── sessions/
//!     ├── sujianwei.session      ← 这场对话里苏见微的记忆
//!     └── zhongshiyu.session
//! ```
//!
//! **同一个 BOT 在 Team A 和 Team B 里各有自己的一份会话。**
//! 这是 D009 那条决策的落地：隔离点在做会话，不在进程。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::error::{io_err, DaemonError};
use crate::{log_debug, log_error, log_info};

/// 原子写（devforge P010 T001 的做法）：临时文件 + rename。
/// 写一半中断不会留下半个 JSON —— meta.json 是暂停/恢复的命根子。
fn atomic_write(path: &Path, content: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, content)?;
    fs::rename(&tmp, path)
}

pub struct TeamStore {
    pub root: PathBuf,
}

impl TeamStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn dir(&self, team_id: &str) -> PathBuf {
        self.root.join(team_id)
    }

    /// 会话文件路径。**有 team 就用 team 作用域，没有就退回全局（兼容）。**
    pub fn session_path(
        &self,
        agents_dir: &Path,
        team_id: Option<&str>,
        bot: &str,
    ) -> PathBuf {
        match team_id {
            Some(tid) => self.dir(tid).join("sessions").join(format!("{}.session", bot)),
            None => agents_dir.join(bot).join(".session-id"),
        }
    }

    /// 建一个 Team。返回 teamId。
    /// 建 Team。`origin` 是发起者的名字（主会话），会被登记成第一个成员。
    pub fn create(&self, label: Option<&str>, origin: Option<&str>) -> Result<String, DaemonError> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let team_id = format!("T-{:08x}", (nanos as u64) & 0xffff_ffff);
        let dir = self.dir(&team_id);

        fs::create_dir_all(dir.join("sessions")).map_err(|e| io_err("创建 Team 目录", e))?;
        // 工作区（M1，抄 devforge 黑板）：这个 Team 的所有产出落这里。
        // findings=检索报告 reports=验证/审查报告 deliverables=开发交付。
        // 物理上不强制读写权限——靠各 BOT 人设约束路径。
        for sub in ["workspace/findings", "workspace/reports", "workspace/deliverables"] {
            fs::create_dir_all(dir.join(sub)).map_err(|e| io_err("创建 Team 工作区", e))?;
        }

        // 发起者永远是第一个成员（D010：主会话也是成员，只是职能不同）
        let members: Vec<Value> = match origin {
            Some(name) => vec![json!({
                "name": name,
                "role": "captain",
                "kind": "session",
                "joinedAt": now_stamp(),
            })],
            None => vec![],
        };

        let meta = json!({
            "teamId": team_id,
            "createdAt": now_stamp(),
            "label": label,
            "members": members,
        });
        fs::write(dir.join("meta.json"), meta.to_string())
            .map_err(|e| io_err("写 Team meta", e))?;

        log_info!("team.created", team = &team_id, label = label.unwrap_or("-"));
        Ok(team_id)
    }

    /// 这个 Team 存在吗。
    pub fn exists(&self, team_id: &str) -> bool {
        self.dir(team_id).join("meta.json").is_file()
    }

    /// 各 BOT 的会话是否存在于该 Team。
    pub fn sessions(&self, team_id: &str) -> Vec<String> {
        let mut out = Vec::new();
        let Ok(rd) = fs::read_dir(self.dir(team_id).join("sessions")) else {
            return out;
        };
        for entry in rd.flatten() {
            if let Some(stem) = entry.path().file_stem().and_then(|s| s.to_str()) {
                out.push(stem.to_string());
            }
        }
        out.sort();
        out
    }

    /// 列所有 Team（新的在前）。
    pub fn list(&self, working_lookup: impl Fn(&str) -> Vec<Value>, include_archived: bool) -> Value {
        let mut teams: Vec<Value> = Vec::new();
        let Ok(entries) = fs::read_dir(&self.root) else {
            return json!({ "teams": teams });
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(team_id) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let meta = fs::read_to_string(path.join("meta.json"))
                .ok()
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok());
            let archived = meta.as_ref().and_then(|m| m.get("archivedAt")).is_some();
            if archived && !include_archived {
                continue;
            }
            let mut members = meta
                .as_ref()
                .and_then(|m| m.get("members"))
                .and_then(|m| m.as_array())
                .cloned()
                .unwrap_or_default();
            // 把「有过会话的 BOT」补成成员（它们是被调用后才出现的）
            for bot in self.sessions(team_id) {
                if !members
                    .iter()
                    .any(|m| m.get("name").and_then(|n| n.as_str()) == Some(bot.as_str()))
                {
                    members.push(json!({ "name": bot, "role": "worker", "kind": "bot" }));
                }
            }
            teams.push(json!({
                "teamId": team_id,
                "label": meta.as_ref().and_then(|m| m.get("label")).cloned().unwrap_or(Value::Null),
                "createdAt": meta.as_ref().and_then(|m| m.get("createdAt")).cloned().unwrap_or(Value::Null),
                "archivedAt": meta.as_ref().and_then(|m| m.get("archivedAt")).cloned().unwrap_or(Value::Null),
                "members": members,
                "sessions": self.sessions(team_id),
                "working": working_lookup(team_id),
            }));
        }
        teams.sort_by(|a, b| {
            b.get("createdAt")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .cmp(a.get("createdAt").and_then(|v| v.as_str()).unwrap_or(""))
        });
        json!({ "teams": teams })
    }

    /// 读会话 id。
    pub fn read_session(&self, path: &Path) -> Option<String> {
        let raw = fs::read_to_string(path).ok()?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }

    /// 写会话 id（父目录不存在就建）。
    pub fn write_session(&self, path: &Path, session_id: &str) {
        if let Some(parent) = path.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                log_error!("session.mkdir_failed", path = &parent.display(), error = e);
                return;
            }
        }
        if let Err(e) = fs::write(path, format!("{}\n", session_id)) {
            log_error!("session.write_failed", path = &path.display(), error = e);
        } else {
            log_debug!("session.saved", path = &path.display(), session = session_id);
        }
    }

    /// 这个 Team 暂停了吗。
    pub fn is_paused(&self, team_id: &str) -> bool {
        let meta_path = self.dir(team_id).join("meta.json");
        let Ok(raw) = fs::read_to_string(meta_path) else {
            return false;
        };
        let Ok(meta) = serde_json::from_str::<Value>(&raw) else {
            return false;
        };
        meta.get("pausedAt").map(|v| !v.is_null()).unwrap_or(false)
    }

    /// 暂停一个 Team。
    ///
    /// ## 暂停的到底是什么
    ///
    /// 在「BOT 用完即退」的架构下，没有常驻进程可杀。所以「暂停」的对象是
    /// **这个 Team 的状态**：
    ///
    ///   1. 把当前会话指纹、成员、进行中的任务**序列化进 meta.json**
    ///   2. 标记 `pausedAt`
    ///   3. 之后任何 `/run` 打进来都拒绝（`TEAM_PAUSED`）
    ///
    /// 恢复时把这份快照读回来、清掉 `pausedAt`，会话 id 原样还在，
    /// **BOT 接着上次的话继续说**。
    ///
    /// 这跟 D001「序列化到磁盘再杀」是同一个思路：**杀的时机不同，快照的内容不变**。
    pub fn pause(&self, team_id: &str) -> Result<Value, DaemonError> {
        let meta_path = self.dir(team_id).join("meta.json");
        if !meta_path.is_file() {
            return Err(DaemonError::TeamNotFound {
                team_id: team_id.to_string(),
            });
        }
        let raw = fs::read_to_string(&meta_path).map_err(|e| io_err("读 Team meta", e))?;
        let mut meta: Value = serde_json::from_str(&raw).map_err(|e| DaemonError::Internal {
            context: "解析 Team meta",
            reason: e.to_string(),
        })?;

        // 序列化：会话指纹（每个 BOT 的 session id）
        let mut sessions_fp: serde_json::Map<String, Value> = serde_json::Map::new();
        for bot in self.sessions(team_id) {
            let path = self.session_path_from(&self.dir(team_id), team_id, &bot);
            if let Some(sid) = self.read_session(&path) {
                sessions_fp.insert(bot, Value::String(sid));
            }
        }

        let snapshot_at = now_stamp();
        if let Some(obj) = meta.as_object_mut() {
            obj.insert("pausedAt".into(), Value::String(snapshot_at.clone()));
            obj.insert(
                "snapshot".into(),
                json!({
                    "at": snapshot_at,
                    "sessions": sessions_fp,
                }),
            );
        }
        atomic_write(&meta_path, &meta.to_string()).map_err(|e| io_err("写 Team meta", e))?;
        crate::log_info!("team.paused", team = team_id);
        Ok(json!({ "teamId": team_id, "pausedAt": snapshot_at }))
    }

    /// 恢复一个 Team：清掉暂停标记，会话快照保留在 meta 里可查。
    pub fn resume(&self, team_id: &str) -> Result<Value, DaemonError> {
        let meta_path = self.dir(team_id).join("meta.json");
        if !meta_path.is_file() {
            return Err(DaemonError::TeamNotFound {
                team_id: team_id.to_string(),
            });
        }
        let raw = fs::read_to_string(&meta_path).map_err(|e| io_err("读 Team meta", e))?;
        let mut meta: Value = serde_json::from_str(&raw).map_err(|e| DaemonError::Internal {
            context: "解析 Team meta",
            reason: e.to_string(),
        })?;

        let restored = match meta.get("snapshot") {
            Some(snap) => snap
                .get("sessions")
                .map(|v| v.clone())
                .unwrap_or(Value::Null),
            None => Value::Null,
        };

        if let Some(obj) = meta.as_object_mut() {
            obj.insert("pausedAt".into(), Value::Null);
            obj.insert("lastResumedAt".into(), Value::String(now_stamp()));
        }
        atomic_write(&meta_path, &meta.to_string()).map_err(|e| io_err("写 Team meta", e))?;
        crate::log_info!("team.resumed", team = team_id);
        Ok(json!({
            "teamId": team_id,
            "restoredSessions": restored,
        }))
    }

    /// 会话路径（从 Team 目录推，不依赖 agents_dir）。
    fn session_path_from(&self, _team_dir: &Path, team_id: &str, bot: &str) -> PathBuf {
        self.dir(team_id).join("sessions").join(format!("{}.session", bot))
    }

    /// 归档一个 Team：从活跃列表移走，但**目录和会话都留着**。
    ///
    /// 做法是在 meta 里打一个 `archivedAt`。列表默认过滤掉归档的，
    /// 传 `includeArchived` 才看得到。
    pub fn archive(&self, team_id: &str) -> Result<(), DaemonError> {
        let meta_path = self.dir(team_id).join("meta.json");
        if !meta_path.is_file() {
            return Err(DaemonError::TeamNotFound {
                team_id: team_id.to_string(),
            });
        }
        let raw = fs::read_to_string(&meta_path).map_err(|e| io_err("读 Team meta", e))?;
        let mut meta: Value =
            serde_json::from_str(&raw).map_err(|e| DaemonError::Internal {
                context: "解析 Team meta",
                reason: e.to_string(),
            })?;
        if let Some(obj) = meta.as_object_mut() {
            obj.insert("archivedAt".into(), Value::String(now_stamp()));
        }
        atomic_write(&meta_path, &meta.to_string()).map_err(|e| io_err("写 Team meta", e))?;
        crate::log_info!("team.archived", team = team_id);
        Ok(())
    }

    /// 取消归档。
    pub fn unarchive(&self, team_id: &str) -> Result<(), DaemonError> {
        let meta_path = self.dir(team_id).join("meta.json");
        if !meta_path.is_file() {
            return Err(DaemonError::TeamNotFound {
                team_id: team_id.to_string(),
            });
        }
        let raw = fs::read_to_string(&meta_path).map_err(|e| io_err("读 Team meta", e))?;
        let mut meta: Value =
            serde_json::from_str(&raw).map_err(|e| DaemonError::Internal {
                context: "解析 Team meta",
                reason: e.to_string(),
            })?;
        if let Some(obj) = meta.as_object_mut() {
            obj.remove("archivedAt");
        }
        atomic_write(&meta_path, &meta.to_string()).map_err(|e| io_err("写 Team meta", e))?;
        crate::log_info!("team.unarchived", team = team_id);
        Ok(())
    }
}

/// 简易时间戳（不引 chrono）。
pub fn now_stamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("epoch:{}", secs)
}

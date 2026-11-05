//! 群：磁盘上的目录 + meta，以及会话的归属。
//!
//! ## 一个 群 是什么
//!
//! 不是一个进程，是**一份命名空间**：
//!
//! ```text
//! ~/.dsh/groups/G-413ce528/
//! ├── meta.json
//! └── sessions/
//!     ├── sujianwei.session      ← 这场对话里苏见微的记忆
//!     └── zhongshiyu.session
//! ```
//!
//! **同一个 BOT 在 群 A 和 群 B 里各有自己的一份会话。**
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

pub struct GroupStore {
    pub root: PathBuf,
}

impl GroupStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn dir(&self, group_id: &str) -> PathBuf {
        self.root.join(group_id)
    }

    /// 会话文件路径。**有群就用群作用域，没有就退回全局（兼容）。**
    pub fn session_path(
        &self,
        agents_dir: &Path,
        group_id: Option<&str>,
        bot: &str,
    ) -> PathBuf {
        match group_id {
            Some(tid) => self.dir(tid).join("sessions").join(format!("{}.session", bot)),
            None => agents_dir.join(bot).join(".session-id"),
        }
    }

    /// 建一个群。返回 groupId。
    /// 建群。`origin` 是发起者的名字（主会话），会被登记成第一个成员。
    pub fn create(&self, label: Option<&str>, origin: Option<&str>) -> Result<String, DaemonError> {
        self.create_with_session(label, origin, None)
    }

    /// 建群并记下「哪个会话开的」（D020：群由会话生）。
    pub fn create_with_session(
        &self,
        label: Option<&str>,
        origin: Option<&str>,
        origin_session: Option<&str>,
    ) -> Result<String, DaemonError> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let group_id = format!("G-{:08x}", (nanos as u64) & 0xffff_ffff);
        let dir = self.dir(&group_id);

        fs::create_dir_all(dir.join("sessions")).map_err(|e| io_err("创建 群 目录", e))?;
        // 工作区（M1，抄 devforge 黑板）：这个 群 的所有产出落这里。
        // findings=检索报告 reports=验证/审查报告 deliverables=开发交付。
        // 物理上不强制读写权限——靠各 BOT 人设约束路径。
        for sub in ["workspace/findings", "workspace/reports", "workspace/deliverables"] {
            fs::create_dir_all(dir.join(sub)).map_err(|e| io_err("创建 群 工作区", e))?;
        }

        // 发起者永远是第一个成员（D010：主会话也是成员，只是职能不同）
        let members: Vec<Value> = match origin {
            Some(name) => vec![json!({
                "name": name,
                "role": "owner",
                "kind": "session",
                "joinedAt": now_stamp(),
            })],
            None => vec![],
        };

        let mut meta = json!({
            "groupId": group_id,
            "createdAt": now_stamp(),
            "label": label,
            "members": members,
        });
        if let Some(sid) = origin_session {
            // 会话删掉之后群还能解释自己从哪来（D020 补 / D029）
            meta["originSession"] = json!({ "id": sid, "createdAt": now_stamp() });
        }
        fs::write(dir.join("meta.json"), meta.to_string())
            .map_err(|e| io_err("写 群 meta", e))?;

        log_info!("group.created", group = &group_id, label = label.unwrap_or("-"));
        Ok(group_id)
    }

    /// 这个 群 存在吗。
    pub fn exists(&self, group_id: &str) -> bool {
        self.dir(group_id).join("meta.json").is_file()
    }

    /// 各 BOT 的会话是否存在于该群。
    pub fn sessions(&self, group_id: &str) -> Vec<String> {
        let mut out = Vec::new();
        let Ok(rd) = fs::read_dir(self.dir(group_id).join("sessions")) else {
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
            return json!({ "groups": teams });
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(group_id) = path.file_name().and_then(|n| n.to_str()) else {
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
            for bot in self.sessions(group_id) {
                if !members
                    .iter()
                    .any(|m| m.get("name").and_then(|n| n.as_str()) == Some(bot.as_str()))
                {
                    members.push(json!({ "name": bot, "role": "worker", "kind": "bot" }));
                }
            }
            teams.push(json!({
                "groupId": group_id,
                "label": meta.as_ref().and_then(|m| m.get("label")).cloned().unwrap_or(Value::Null),
                "createdAt": meta.as_ref().and_then(|m| m.get("createdAt")).cloned().unwrap_or(Value::Null),
                "archivedAt": meta.as_ref().and_then(|m| m.get("archivedAt")).cloned().unwrap_or(Value::Null),
                "members": members,
                "sessions": self.sessions(group_id),
                "working": working_lookup(group_id),
            }));
        }
        teams.sort_by(|a, b| {
            b.get("createdAt")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .cmp(a.get("createdAt").and_then(|v| v.as_str()).unwrap_or(""))
        });
        json!({ "groups": teams })
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

    /// 认领一个旧群（D033）：把它的 `originSession` 换成当前会话。
    ///
    /// 为什么需要：群的门是会话 id 做的（D020）。开新会话时 `resolve` 找不到旧群，
    /// 于是只能建新的——明明数据都在，就是进不去。认领是那把钥匙。
    ///
    /// 记 `adoptedFrom` 留痕：以后能查"这个群本来是哪个会话开的"。
    pub fn adopt(&self, group_id: &str, session_id: &str) -> Result<Value, DaemonError> {
        let meta_path = self.dir(group_id).join("meta.json");
        let raw = fs::read_to_string(&meta_path).map_err(|e| io_err("读群 meta", e))?;
        let mut meta: Value = serde_json::from_str(&raw).map_err(|e| DaemonError::Internal {
            context: "解析群 meta".into(),
            reason: e.to_string(),
        })?;

        let previous = meta
            .get("originSession")
            .and_then(|o| o.get("id"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        meta["originSession"] = json!({
            "id": session_id,
            "createdAt": now_stamp(),
        });
        if let Some(prev) = previous {
            if prev != session_id {
                meta["adoptedFrom"] = json!([prev]);
            }
        }

        // 群主还是原来那个（D010/D025）——认领换的是"门"，不是"主人"
        atomic_write(&meta_path, &meta.to_string()).map_err(|e| io_err("写群 meta", e))?;

        // 一会话一群（D020）：把"原来指向我这个会话的其它群"让渡掉。
        //
        // 不这么做的话，一个会话会同时拥有好几个群（实测：认领后
        // ['G-aaef5718','T-e59a0660'] 两个都指向同一个 session），
        // 而 resolve 返回哪个取决于目录扫描顺序——**不确定的当前群**。
        let mut released = Vec::new();
        if let Ok(entries) = fs::read_dir(&self.root) {
            for entry in entries.flatten() {
                let dir = entry.path();
                if !dir.is_dir() || dir.file_name().and_then(|n| n.to_str()) == Some(group_id) {
                    continue;
                }
                let other_meta = dir.join("meta.json");
                let Ok(raw) = fs::read_to_string(&other_meta) else { continue };
                let Ok(mut other) = serde_json::from_str::<Value>(&raw) else { continue };
                let points_to_me = other
                    .get("originSession")
                    .and_then(|o| o.get("id"))
                    .and_then(|v| v.as_str())
                    == Some(session_id);
                if !points_to_me {
                    continue;
                }
                // 让渡：清掉 originSession，记一笔"让给谁了"。数据一个不删。
                if let Some(obj) = other.as_object_mut() {
                    obj.remove("originSession");
                    obj.insert(
                        "adoptedOut".into(),
                        json!({ "to": group_id, "at": now_stamp() }),
                    );
                }
                if let Ok(text) = serde_json::to_string(&other) {
                    if atomic_write(&other_meta, &text).is_ok() {
                        if let Some(name) = dir.file_name().and_then(|n| n.to_str()) {
                            released.push(name.to_string());
                        }
                    }
                }
            }
        }
        if !released.is_empty() {
            log_info!("group.released", group = group_id, released = released.join(","));
        }
        log_info!("group.adopted", group = group_id, session = session_id);
        Ok(meta)
    }

    /// 按会话找群（D020：同一个会话 resume 回来就是同一个群）。
    ///
    /// 扫 meta.json 的 `originSession.id`。群数量级是几十，扫一遍足够快。
    pub fn find_by_session(&self, session_id: &str) -> Option<String> {
        let entries = fs::read_dir(&self.root).ok()?;
        for entry in entries.flatten() {
            let path = entry.path().join("meta.json");
            let Ok(raw) = fs::read_to_string(&path) else { continue };
            let Ok(meta) = serde_json::from_str::<Value>(&raw) else { continue };
            if meta
                .get("originSession")
                .and_then(|o| o.get("id"))
                .and_then(|v| v.as_str())
                == Some(session_id)
            {
                return meta
                    .get("groupId")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
            }
        }
        None
    }

    /// 派活即入群（D028）：把一个 BOT 真正写进 `meta.json` 的成员表。
    ///
    /// 为什么不能靠 `list()` 动态补：那个视图是拿「有过会话的 BOT」反推的，
    /// 而会话文件 7 天后会被清掉（D029）——成员会凭空消失。**成员必须是落盘的。**
    ///
    /// 返回 `true` 表示这次是新加入的（调用方可以据此写群史 `member.joined`）。
    pub fn ensure_member(&self, group_id: &str, bot: &str, role: &str) -> bool {
        let meta_path = self.dir(group_id).join("meta.json");
        let Ok(raw) = fs::read_to_string(&meta_path) else {
            return false;
        };
        let Ok(mut meta) = serde_json::from_str::<Value>(&raw) else {
            return false;
        };
        let Some(arr) = meta.get_mut("members").and_then(|m| m.as_array_mut()) else {
            return false;
        };
        if arr
            .iter()
            .any(|m| m.get("name").and_then(|n| n.as_str()) == Some(bot))
        {
            return false;
        }
        arr.push(json!({
            "name": bot,
            "role": role,
            "kind": "bot",
            "joinedAt": now_stamp(),
        }));
        match serde_json::to_string(&meta) {
            Ok(text) => {
                if let Err(e) = atomic_write(&meta_path, &text) {
                    log_error!("member.write_failed", path = &meta_path.display(), error = e);
                    return false;
                }
                log_info!("member.joined", group = group_id, bot = bot, role = role);
                true
            }
            Err(e) => {
                log_error!("member.serialize_failed", error = e);
                false
            }
        }
    }

    /// 长回执落盘（long-reply-fallback.md）：超过阈值的回执全文写 `workspace/replies/`。
    ///
    /// 为什么不是防截断：实测 daemon/插件都不截断（3892 字符完整回来）。
    /// 落盘是为了防**上下文被撑爆**——三万字的回执塞进主会话就完了。
    pub fn write_reply(&self, group_id: &str, run_id: &str, bot: &str, reply: &str) -> Option<String> {
        let rel = format!("workspace/replies/{}-{}.md", run_id, bot);
        let path = self.dir(group_id).join(&rel);
        if let Some(parent) = path.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                log_error!("reply.mkdir_failed", path = &parent.display(), error = e);
                return None;
            }
        }
        let body = format!(
            "---\nrun: {run_id}\nbot: {bot}\ngroup: {group_id}\nchars: {chars}\nat: {at}\n---\n\n{reply}\n",
            run_id = run_id,
            bot = bot,
            group_id = group_id,
            chars = reply.chars().count(),
            at = now_stamp(),
            reply = reply,
        );
        match atomic_write(&path, &body) {
            Ok(_) => Some(rel),
            Err(e) => {
                log_error!("reply.write_failed", path = &path.display(), error = e);
                None
            }
        }
    }

    /// 把异步 job 的结果写进信箱（D019）。
    ///
    /// 路径：`groups/<id>/workspace/mailbox/<jobId>-<bot>.md`，返回**相对群目录**的路径。
    /// 为什么双写：内存里的 `jobs` 表重启就没了，信箱是「进程死了也还在」的那一份。
    pub fn write_mailbox(
        &self,
        group_id: Option<&str>,
        job_id: &str,
        bot: &str,
        reply: &str,
        sender: Option<&str>,
    ) -> Option<String> {
        let gid = group_id?;
        let rel = format!("workspace/mailbox/{}-{}.md", job_id, bot);
        let path = self.dir(gid).join(&rel);
        if let Some(parent) = path.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                log_error!("mailbox.mkdir_failed", path = &parent.display(), error = e);
                return None;
            }
        }
        let body = format!(
            "---\njob: {job_id}\nbot: {bot}\ngroup: {gid}\nfrom: {sender}\nstatus: done\nat: {at}\n---\n\n## 回执\n\n{reply}\n",
            job_id = job_id,
            bot = bot,
            gid = gid,
            sender = sender.unwrap_or("-"),
            at = now_stamp(),
            reply = reply,
        );
        match atomic_write(&path, &body) {
            Ok(_) => {
                log_info!("mailbox.written", group = gid, job = job_id, bot = bot, chars = reply.chars().count());
                Some(rel)
            }
            Err(e) => {
                log_error!("mailbox.write_failed", path = &path.display(), error = e);
                None
            }
        }
    }

    /// 追加一条群史事件（D021：只记事件，不记流水）。
    ///
    /// 落 `groups/<id>/log.jsonl`，一行一个 JSON。**永久保留**（D029 的「账本」类）。
    pub fn append_event(&self, group_id: &str, event: Value) {
        let dir = self.dir(group_id);
        if let Err(e) = fs::create_dir_all(&dir) {
            log_error!("group.mkdir_failed", path = &dir.display(), error = e);
            return;
        }
        let path = dir.join("log.jsonl");
        let line = match serde_json::to_string(&event) {
            Ok(l) => l,
            Err(e) => {
                log_error!("group.event_serialize_failed", error = e);
                return;
            }
        };
        use std::io::Write;
        match fs::OpenOptions::new().create(true).append(true).open(&path) {
            Ok(mut f) => {
                if let Err(e) = writeln!(f, "{}", line) {
                    log_error!("group.event_write_failed", path = &path.display(), error = e);
                }
            }
            Err(e) => log_error!("group.event_open_failed", path = &path.display(), error = e),
        }
    }

    /// 这个群归档了吗（D026：archived 状态下任何人都不该能派活）。
    pub fn is_archived(&self, group_id: &str) -> bool {
        let meta_path = self.dir(group_id).join("meta.json");
        let Ok(raw) = fs::read_to_string(meta_path) else {
            return false;
        };
        let Ok(meta) = serde_json::from_str::<Value>(&raw) else {
            return false;
        };
        meta.get("archivedAt").map(|v| !v.is_null()).unwrap_or(false)
    }

    /// 这个 群 暂停了吗。
    pub fn is_paused(&self, group_id: &str) -> bool {        let meta_path = self.dir(group_id).join("meta.json");
        let Ok(raw) = fs::read_to_string(meta_path) else {
            return false;
        };
        let Ok(meta) = serde_json::from_str::<Value>(&raw) else {
            return false;
        };
        meta.get("pausedAt").map(|v| !v.is_null()).unwrap_or(false)
    }

    /// 暂停一个群。
    ///
    /// ## 暂停的到底是什么
    ///
    /// 在「BOT 用完即退」的架构下，没有常驻进程可杀。所以「暂停」的对象是
    /// **这个 群 的状态**：
    ///
    ///   1. 把当前会话指纹、成员、进行中的任务**序列化进 meta.json**
    ///   2. 标记 `pausedAt`
    ///   3. 之后任何 `/run` 打进来都拒绝（`GROUP_PAUSED`）
    ///
    /// 恢复时把这份快照读回来、清掉 `pausedAt`，会话 id 原样还在，
    /// **BOT 接着上次的话继续说**。
    ///
    /// 这跟 D001「序列化到磁盘再杀」是同一个思路：**杀的时机不同，快照的内容不变**。
    pub fn pause(&self, group_id: &str) -> Result<Value, DaemonError> {
        let meta_path = self.dir(group_id).join("meta.json");
        if !meta_path.is_file() {
            return Err(DaemonError::GroupNotFound {
                group_id: group_id.to_string(),
            });
        }
        let raw = fs::read_to_string(&meta_path).map_err(|e| io_err("读 群 meta", e))?;
        let mut meta: Value = serde_json::from_str(&raw).map_err(|e| DaemonError::Internal {
            context: "解析 群 meta",
            reason: e.to_string(),
        })?;

        // 序列化：会话指纹（每个 BOT 的 session id）
        let mut sessions_fp: serde_json::Map<String, Value> = serde_json::Map::new();
        for bot in self.sessions(group_id) {
            let path = self.session_path_from(&self.dir(group_id), group_id, &bot);
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
        atomic_write(&meta_path, &meta.to_string()).map_err(|e| io_err("写 群 meta", e))?;
        crate::log_info!("group.paused", group = group_id);
        Ok(json!({ "groupId": group_id, "pausedAt": snapshot_at }))
    }

    /// 恢复一个群：清掉暂停标记，会话快照保留在 meta 里可查。
    pub fn resume(&self, group_id: &str) -> Result<Value, DaemonError> {
        let meta_path = self.dir(group_id).join("meta.json");
        if !meta_path.is_file() {
            return Err(DaemonError::GroupNotFound {
                group_id: group_id.to_string(),
            });
        }
        let raw = fs::read_to_string(&meta_path).map_err(|e| io_err("读 群 meta", e))?;
        let mut meta: Value = serde_json::from_str(&raw).map_err(|e| DaemonError::Internal {
            context: "解析 群 meta",
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
        atomic_write(&meta_path, &meta.to_string()).map_err(|e| io_err("写 群 meta", e))?;
        crate::log_info!("group.resumed", group = group_id);
        Ok(json!({
            "groupId": group_id,
            "restoredSessions": restored,
        }))
    }

    /// 会话路径（从 群 目录推，不依赖 agents_dir）。
    fn session_path_from(&self, _team_dir: &Path, group_id: &str, bot: &str) -> PathBuf {
        self.dir(group_id).join("sessions").join(format!("{}.session", bot))
    }

    /// 归档一个群：从活跃列表移走，但**目录和会话都留着**。
    ///
    /// 做法是在 meta 里打一个 `archivedAt`。列表默认过滤掉归档的，
    /// 传 `includeArchived` 才看得到。
    pub fn archive(&self, group_id: &str) -> Result<(), DaemonError> {
        let meta_path = self.dir(group_id).join("meta.json");
        if !meta_path.is_file() {
            return Err(DaemonError::GroupNotFound {
                group_id: group_id.to_string(),
            });
        }
        let raw = fs::read_to_string(&meta_path).map_err(|e| io_err("读 群 meta", e))?;
        let mut meta: Value =
            serde_json::from_str(&raw).map_err(|e| DaemonError::Internal {
                context: "解析 群 meta",
                reason: e.to_string(),
            })?;
        if let Some(obj) = meta.as_object_mut() {
            obj.insert("archivedAt".into(), Value::String(now_stamp()));
        }
        atomic_write(&meta_path, &meta.to_string()).map_err(|e| io_err("写 群 meta", e))?;
        crate::log_info!("group.archived", group = group_id);
        Ok(())
    }

    /// 取消归档。
    pub fn unarchive(&self, group_id: &str) -> Result<(), DaemonError> {
        let meta_path = self.dir(group_id).join("meta.json");
        if !meta_path.is_file() {
            return Err(DaemonError::GroupNotFound {
                group_id: group_id.to_string(),
            });
        }
        let raw = fs::read_to_string(&meta_path).map_err(|e| io_err("读 群 meta", e))?;
        let mut meta: Value =
            serde_json::from_str(&raw).map_err(|e| DaemonError::Internal {
                context: "解析 群 meta",
                reason: e.to_string(),
            })?;
        if let Some(obj) = meta.as_object_mut() {
            obj.remove("archivedAt");
        }
        atomic_write(&meta_path, &meta.to_string()).map_err(|e| io_err("写 群 meta", e))?;
        crate::log_info!("group.unarchived", group = group_id);
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

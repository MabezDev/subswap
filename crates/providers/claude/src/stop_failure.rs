//! Claude Code `StopFailure` hook：真实请求被额度拒绝时的上报入口。
//!
//! usage 端点不一定报出所有限额（Team 席位实测没有 `seven_day`），Claude Code 的 429 才是
//! 最终裁决。hook 收到 `error = "rate_limit"` 后，从会话 transcript 尾部读出该次拒绝的
//! `quotaLimits`（上游给的 `resetsAt` / `rateLimitType`），交给调用方记录。
//!
//! 安装位置是用户级 `settings.json` 的 `hooks.StopFailure`；`subswap run claude` 的隔离目录
//! 复制共享设置，隔离会话同样会触发，此时 `CLAUDE_CONFIG_DIR` 指向隔离目录。

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, TimeZone, Utc};
use serde::Deserialize;
use subswap_core::error::{Error, Result};

use crate::claude_files::{read_oauth_account, read_settings};
use crate::paths::{claude_home, global_config_path, settings_path};

/// 写进 `settings.json` 的 hook 命令。识别已安装条目也靠它。
pub const HOOK_SUBCOMMAND: &str = "hook claude-stop-failure";

/// transcript 里的拒绝记录离 hook 触发超过这么久，就不是本次拒绝（transcript 可能尚未落盘）。
const MAX_TRANSCRIPT_LAG_SECS: i64 = 10 * 60;

/// 只读 transcript 尾部这么多字节；拒绝记录总在末尾。
const TRANSCRIPT_TAIL_BYTES: u64 = 512 * 1024;

/// `StopFailure` hook 的 stdin。只取用得到的字段。
#[derive(Debug, Default, Deserialize)]
pub struct HookInput {
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub transcript_path: Option<PathBuf>,
}

impl HookInput {
    /// 是否额度类拒绝（其他失败如网络、鉴权不代表账号额度用完）。
    pub fn is_rate_limit(&self) -> bool {
        self.error.as_deref() == Some("rate_limit")
    }
}

/// 从 transcript 读出的一次拒绝。
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptRejection {
    pub rejected_at: DateTime<Utc>,
    /// `quotaLimits.resetsAt`；Fable 等按模型限额的拒绝没有结构化字段。
    pub reset_at: Option<DateTime<Utc>>,
    /// `quotaLimits.rateLimitType`，如 `five_hour` / `seven_day`。
    pub kind: Option<String>,
}

/// 在 transcript 尾部找最近一条 `rate_limit` 拒绝；太旧（不是本次）或找不到返回 `None`。
pub fn scan_transcript(path: &Path, now: DateTime<Utc>) -> Option<TranscriptRejection> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(TRANSCRIPT_TAIL_BYTES)))
        .ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    text.lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|v| {
            v.get("isApiErrorMessage")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
                && v.get("error").and_then(serde_json::Value::as_str) == Some("rate_limit")
        })
        .and_then(|v| {
            let rejected_at = v
                .get("timestamp")?
                .as_str()?
                .parse::<DateTime<Utc>>()
                .ok()?;
            let limits = v.get("quotaLimits");
            Some(TranscriptRejection {
                rejected_at,
                reset_at: limits
                    .and_then(|l| l.get("resetsAt")?.as_i64())
                    .and_then(|secs| Utc.timestamp_opt(secs, 0).single()),
                kind: limits
                    .and_then(|l| l.get("rateLimitType")?.as_str())
                    .map(str::to_string),
            })
        })
        .filter(|r| now - r.rejected_at <= Duration::seconds(MAX_TRANSCRIPT_LAG_SECS))
}

/// hook 所在会话实际使用的 Claude 目录（隔离会话为隔离目录）。
pub fn session_claude_home() -> PathBuf {
    claude_home()
}

/// 某 Claude 目录当前登录账号的邮箱（即 subswap 的账号 id）。
pub fn signed_in_email(home: &Path) -> Result<Option<String>> {
    Ok(read_oauth_account(&global_config_path(home))?.map(|a| a.email_address))
}

/// `settings.json` 里是否已有 subswap 的 `StopFailure` hook。
pub fn hook_installed(home: &Path) -> Result<bool> {
    let settings = read_settings(&settings_path(home))?;
    Ok(stop_failure_groups(&settings)
        .iter()
        .any(group_has_subswap_hook))
}

/// 安装 hook；已安装返回 `false` 不改文件。保留 settings.json 其余内容。
pub fn install_hook(home: &Path, command: &str) -> Result<bool> {
    let path = settings_path(home);
    let mut settings = read_settings(&path)?;
    if stop_failure_groups(&settings)
        .iter()
        .any(group_has_subswap_hook)
    {
        return Ok(false);
    }
    let root = settings
        .as_object_mut()
        .ok_or_else(|| not_object(&path, "root"))?;
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or_else(|| not_object(&path, "hooks"))?;
    let groups = hooks
        .entry("StopFailure")
        .or_insert_with(|| serde_json::json!([]))
        .as_array_mut()
        .ok_or_else(|| not_object(&path, "hooks.StopFailure"))?;
    groups.push(serde_json::json!({
        "hooks": [{ "type": "command", "command": command }]
    }));
    crate::claude_files::write_settings(&path, &settings)?;
    Ok(true)
}

/// 卸载 hook；未安装返回 `false`。只删 subswap 自己的条目，清掉因此变空的容器。
pub fn uninstall_hook(home: &Path) -> Result<bool> {
    let path = settings_path(home);
    let mut settings = read_settings(&path)?;
    let Some(hooks) = settings
        .get_mut("hooks")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return Ok(false);
    };
    let Some(groups) = hooks
        .get_mut("StopFailure")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return Ok(false);
    };
    let mut changed = false;
    for group in groups.iter_mut() {
        if let Some(list) = group
            .get_mut("hooks")
            .and_then(serde_json::Value::as_array_mut)
        {
            let before = list.len();
            list.retain(|h| !is_subswap_hook(h));
            changed |= list.len() != before;
        }
    }
    if !changed {
        return Ok(false);
    }
    groups.retain(|g| {
        g.get("hooks")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|l| !l.is_empty())
    });
    if groups.is_empty() {
        hooks.remove("StopFailure");
    }
    if hooks.is_empty() {
        if let Some(root) = settings.as_object_mut() {
            root.remove("hooks");
        }
    }
    crate::claude_files::write_settings(&path, &settings)?;
    Ok(true)
}

fn stop_failure_groups(settings: &serde_json::Value) -> Vec<serde_json::Value> {
    settings
        .get("hooks")
        .and_then(|h| h.get("StopFailure"))
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn group_has_subswap_hook(group: &serde_json::Value) -> bool {
    group
        .get("hooks")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|l| l.iter().any(is_subswap_hook))
}

fn is_subswap_hook(hook: &serde_json::Value) -> bool {
    hook.get("command")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|c| c.contains(HOOK_SUBCOMMAND))
}

fn not_object(path: &Path, field: &str) -> Error {
    Error::Provider(format!(
        "Claude settings {} {field} has an unexpected type",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_transcript(dir: &Path, lines: &[serde_json::Value]) -> PathBuf {
        let path = dir.join("session.jsonl");
        let body: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        std::fs::write(&path, body.join("\n") + "\n").unwrap();
        path
    }

    /// 2026-09 实样：周上限拒绝带结构化 `quotaLimits`。
    #[test]
    fn scans_structured_weekly_rejection() {
        let tmp = tempfile::tempdir().unwrap();
        let now = Utc::now();
        let path = write_transcript(
            tmp.path(),
            &[
                serde_json::json!({"type": "user", "timestamp": now.to_rfc3339()}),
                serde_json::json!({
                    "type": "assistant",
                    "timestamp": now.to_rfc3339(),
                    "isApiErrorMessage": true,
                    "error": "rate_limit",
                    "apiErrorStatus": 429,
                    "quotaLimits": {
                        "status": "rejected", "resetsAt": 1791000000,
                        "rateLimitType": "seven_day", "overageStatus": "rejected"
                    },
                    "message": {"content": [{"type": "text",
                        "text": "You've hit your weekly limit · resets Oct 3, 5am (Europe/London)"}]}
                }),
            ],
        );
        let r = scan_transcript(&path, now).unwrap();
        assert_eq!(r.kind.as_deref(), Some("seven_day"));
        assert_eq!(r.reset_at.unwrap().timestamp(), 1791000000);
    }

    /// Fable 限额拒绝没有 `quotaLimits`：仍识别，恢复时间留空交给兜底封锁。
    #[test]
    fn scans_unstructured_rejection_without_reset() {
        let tmp = tempfile::tempdir().unwrap();
        let now = Utc::now();
        let path = write_transcript(
            tmp.path(),
            &[serde_json::json!({
                "timestamp": now.to_rfc3339(),
                "isApiErrorMessage": true,
                "error": "rate_limit",
            })],
        );
        let r = scan_transcript(&path, now).unwrap();
        assert_eq!(r.reset_at, None);
        assert_eq!(r.kind, None);
    }

    #[test]
    fn ignores_stale_and_non_rate_limit_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let now = Utc::now();
        let path = write_transcript(
            tmp.path(),
            &[
                serde_json::json!({
                    "timestamp": (now - Duration::hours(2)).to_rfc3339(),
                    "isApiErrorMessage": true, "error": "rate_limit",
                }),
                serde_json::json!({
                    "timestamp": now.to_rfc3339(),
                    "isApiErrorMessage": true, "error": "server_error",
                }),
            ],
        );
        assert_eq!(scan_transcript(&path, now), None);
        assert_eq!(
            scan_transcript(&tmp.path().join("missing.jsonl"), now),
            None
        );
    }

    #[test]
    fn install_is_idempotent_and_preserves_other_hooks() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        std::fs::write(
            settings_path(home),
            serde_json::json!({
                "model": "opus",
                "hooks": {
                    "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "gate"}]}],
                    "StopFailure": [{"hooks": [{"type": "command", "command": "notify"}]}]
                }
            })
            .to_string(),
        )
        .unwrap();

        assert!(!hook_installed(home).unwrap());
        assert!(install_hook(home, "subswap hook claude-stop-failure").unwrap());
        assert!(!install_hook(home, "subswap hook claude-stop-failure").unwrap());
        assert!(hook_installed(home).unwrap());
        let s = read_settings(&settings_path(home)).unwrap();
        assert_eq!(s["model"], "opus");
        assert_eq!(s["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "gate");
        assert_eq!(s["hooks"]["StopFailure"].as_array().unwrap().len(), 2);

        assert!(uninstall_hook(home).unwrap());
        assert!(!uninstall_hook(home).unwrap());
        let s = read_settings(&settings_path(home)).unwrap();
        assert_eq!(
            s["hooks"]["StopFailure"][0]["hooks"][0]["command"],
            "notify"
        );
        assert_eq!(s["hooks"]["StopFailure"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn uninstall_removes_empty_containers() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        install_hook(home, "/opt/bin/subswap hook claude-stop-failure").unwrap();
        uninstall_hook(home).unwrap();
        let s = read_settings(&settings_path(home)).unwrap();
        assert!(s.get("hooks").is_none(), "{s}");
    }

    #[test]
    fn hook_input_only_rate_limit_counts() {
        let rl: HookInput =
            serde_json::from_str(r#"{"error": "rate_limit", "transcript_path": "/x"}"#).unwrap();
        assert!(rl.is_rate_limit());
        let other: HookInput = serde_json::from_str(r#"{"error": "overloaded"}"#).unwrap();
        assert!(!other.is_rate_limit());
    }
}

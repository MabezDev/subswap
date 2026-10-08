//! 原生客户端上报的「请求被额度拒绝」记录。
//!
//! usage 端点不一定报出所有限额（如 Team 席位的周上限），客户端真实请求被 429 拒绝才是
//! 最终裁决。Provider 专属的上报入口（如 Claude Code `StopFailure` hook）调用
//! [`record_rejection`] 落盘；自动切换决策与默认入口展示前用 [`RejectionStore::apply`]
//! 把仍在封锁期的记录折算成一个 `Rejected` 耗尽窗口，切走、候选、回退都按耗尽处理。
//!
//! - **落盘而非内存**：hook、CLI、daemon 是不同进程。
//! - **不进 quota 缓存**：缓存只存 usage 端点原样结果，封锁在决策 / 展示时叠加。
//! - 读取 fail-open：缺失 / 损坏视为没有记录。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::auto_policy::{AccountWithQuotas, ProviderSnapshot};
use crate::error::Result;
use crate::model::{AccountId, Quota, QuotaStatus, QuotaWindow};
use crate::paths::AppPaths;

/// 一次被拒记录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rejection {
    /// 被拒请求的时间。
    pub rejected_at: DateTime<Utc>,
    /// 上游给出的恢复时间；缺失时按 `auto_swap.rejection_block_ms` 封锁。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<DateTime<Utc>>,
    /// 上游限额类型，如 `five_hour` / `seven_day`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

impl Rejection {
    /// 封锁截止时间。
    pub fn blocked_until(&self, fallback_block_ms: i64) -> DateTime<Utc> {
        self.reset_at
            .unwrap_or_else(|| self.rejected_at + Duration::milliseconds(fallback_block_ms.max(0)))
    }
}

/// 某一时刻的被拒记录快照。
#[derive(Debug, Default, Clone)]
pub struct RejectionStore {
    entries: BTreeMap<String, Rejection>,
}

impl RejectionStore {
    /// 读默认路径；任何失败都返回空记录。
    pub fn load() -> Self {
        AppPaths::resolve()
            .map(|p| Self::load_from(&p.rejections_file()))
            .unwrap_or_default()
    }

    pub fn load_from(path: &Path) -> Self {
        Self {
            entries: read_entries(path),
        }
    }

    /// 仍在封锁期内的记录。
    pub fn active(&self, provider: &str, id: &AccountId, now: DateTime<Utc>) -> Option<&Rejection> {
        let block_ms = crate::settings::current().auto_swap.rejection_block_ms;
        self.entries
            .get(&key(provider, id))
            .filter(|r| r.blocked_until(block_ms) > now)
    }

    /// 仍在封锁期内时，折算成的 `Rejected` 耗尽窗口。
    pub fn quota_for(&self, provider: &str, id: &AccountId, now: DateTime<Utc>) -> Option<Quota> {
        let block_ms = crate::settings::current().auto_swap.rejection_block_ms;
        let r = self.active(provider, id, now)?;
        Some(Quota {
            provider: provider.into(),
            account_id: id.clone(),
            window: QuotaWindow::Rejected,
            used: 100,
            limit: 100,
            reset_at: Some(r.blocked_until(block_ms)),
            status: QuotaStatus::Exhausted,
            note: r.kind.clone(),
        })
    }

    /// 给快照里仍在封锁期的账号追加 `Rejected` 窗口；不改原快照。
    pub fn apply(&self, snapshot: &ProviderSnapshot, now: DateTime<Utc>) -> ProviderSnapshot {
        ProviderSnapshot {
            accounts: snapshot
                .accounts
                .iter()
                .map(|a| AccountWithQuotas {
                    quotas: self.with_rejection(&a.account.provider, &a.account.id, &a.quotas, now),
                    ..a.clone()
                })
                .collect(),
            ..snapshot.clone()
        }
    }

    /// `quotas` 加上该账号仍在封锁期的 `Rejected` 窗口（若有）。
    pub fn with_rejection(
        &self,
        provider: &str,
        id: &AccountId,
        quotas: &[Quota],
        now: DateTime<Utc>,
    ) -> Vec<Quota> {
        let mut out = quotas.to_vec();
        out.extend(self.quota_for(provider, id, now));
        out
    }
}

/// 记录一次被拒（覆盖该账号旧记录），顺带清掉已过期的记录。
pub fn record_rejection(provider: &str, id: &AccountId, rejection: Rejection) -> Result<()> {
    record_rejection_at(
        &AppPaths::resolve()?.rejections_file(),
        provider,
        id,
        rejection,
    )
}

/// 清掉某账号的被拒记录（用户手动切到它时调用：显式选择即表示要重试）。
pub fn clear_rejection(provider: &str, id: &AccountId) -> Result<()> {
    clear_rejection_at(&AppPaths::resolve()?.rejections_file(), provider, id)
}

pub fn record_rejection_at(
    path: &Path,
    provider: &str,
    id: &AccountId,
    rejection: Rejection,
) -> Result<()> {
    update(path, |entries| {
        entries.insert(key(provider, id), rejection);
    })
}

pub fn clear_rejection_at(path: &Path, provider: &str, id: &AccountId) -> Result<()> {
    update(path, |entries| {
        entries.remove(&key(provider, id));
    })
}

fn key(provider: &str, id: &AccountId) -> String {
    format!("{provider}/{}", id.0)
}

fn read_entries(path: &Path) -> BTreeMap<String, Rejection> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// hook 可能与 CLI / 另一个 hook 并发写；读改写全程持独占锁。
fn update(path: &Path, f: impl FnOnce(&mut BTreeMap<String, Rejection>)) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path(path))?;
    lock.lock_exclusive()?;
    let mut entries = read_entries(path);
    f(&mut entries);
    let block_ms = crate::settings::current().auto_swap.rejection_block_ms;
    let now = Utc::now();
    entries.retain(|_, r| r.blocked_until(block_ms) > now);
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string_pretty(&entries)?)?;
    std::fs::rename(&tmp, path)?;
    let _ = FileExt::unlock(&lock);
    Ok(())
}

fn lock_path(path: &Path) -> PathBuf {
    path.with_extension("json.lock")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auto_policy::QuotaFetchState;
    use crate::model::{Account, QuotaPoolSemantics};

    fn rejection(hours_until_reset: Option<i64>) -> Rejection {
        let now = Utc::now();
        Rejection {
            rejected_at: now,
            reset_at: hours_until_reset.map(|h| now + Duration::hours(h)),
            kind: Some("seven_day".into()),
        }
    }

    fn id(s: &str) -> AccountId {
        AccountId(s.into())
    }

    #[test]
    fn recorded_rejection_becomes_exhausted_window_until_reset() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rejections.json");
        record_rejection_at(&path, "claude", &id("work"), rejection(Some(30))).unwrap();

        let store = RejectionStore::load_from(&path);
        let now = Utc::now();
        let q = store.quota_for("claude", &id("work"), now).unwrap();
        assert_eq!(q.window, QuotaWindow::Rejected);
        assert_eq!(q.status, QuotaStatus::Exhausted);
        assert_eq!(q.limit, 100);
        assert_eq!(q.note.as_deref(), Some("seven_day"));
        assert!(store.quota_for("claude", &id("other"), now).is_none());
        assert!(store
            .quota_for("claude", &id("work"), now + Duration::hours(31))
            .is_none());
    }

    #[test]
    fn missing_reset_falls_back_to_block_duration() {
        let r = Rejection {
            reset_at: None,
            ..rejection(None)
        };
        assert_eq!(
            r.blocked_until(60_000),
            r.rejected_at + Duration::minutes(1)
        );
    }

    #[test]
    fn clear_removes_and_expired_entries_are_pruned() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rejections.json");
        record_rejection_at(&path, "claude", &id("a"), rejection(Some(5))).unwrap();
        record_rejection_at(&path, "claude", &id("b"), rejection(Some(-1))).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("claude/a") && !raw.contains("claude/b"),
            "{raw}"
        );
        clear_rejection_at(&path, "claude", &id("a")).unwrap();
        assert!(RejectionStore::load_from(&path)
            .active("claude", &id("a"), Utc::now())
            .is_none());
    }

    #[test]
    fn corrupt_file_reads_as_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rejections.json");
        std::fs::write(&path, "{not json").unwrap();
        assert!(RejectionStore::load_from(&path)
            .quota_for("claude", &id("a"), Utc::now())
            .is_none());
    }

    #[test]
    fn apply_appends_window_only_to_rejected_account() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rejections.json");
        record_rejection_at(&path, "claude", &id("work"), rejection(Some(30))).unwrap();
        let mk = |name: &str| AccountWithQuotas {
            account: Account {
                provider: "claude".into(),
                id: id(name),
                label: name.into(),
                active: name == "work",
                created_at: Utc::now(),
                last_used_at: None,
                priority: 100,
                reserve_pct: 0,
                extra: serde_json::Map::new(),
            },
            quotas: Vec::new(),
            fetch_state: QuotaFetchState::Ready,
        };
        let snap = ProviderSnapshot {
            provider: "claude".into(),
            pool_semantics: QuotaPoolSemantics::Stacked,
            accounts: vec![mk("work"), mk("personal")],
        };
        let applied = RejectionStore::load_from(&path).apply(&snap, Utc::now());
        assert_eq!(applied.accounts[0].quotas.len(), 1);
        assert!(applied.accounts[1].quotas.is_empty());
        assert!(snap.accounts[0].quotas.is_empty());
    }
}

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
//!
//! 解封时间见 [`unblock_at`]：上游给了恢复时间就用它，否则按该账号已知的周重置时刻推算。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::auto_policy::{AccountWithQuotas, ProviderSnapshot};
use crate::error::Result;
use crate::model::{Account, AccountId, Quota, QuotaStatus, QuotaWindow};
use crate::paths::AppPaths;

/// 一次被拒记录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rejection {
    /// 被拒请求的时间。
    pub rejected_at: DateTime<Utc>,
    /// 上游给出的恢复时间；缺失时按 [`unblock_at`] 推算。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<DateTime<Utc>>,
    /// 上游限额类型，如 `five_hour` / `seven_day`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct StoreFile {
    #[serde(default)]
    rejections: BTreeMap<String, Rejection>,
    /// 从带恢复时间的周限额拒绝里学到的某一次周重置时刻，按 7 天步长外推。
    #[serde(default)]
    weekly_anchors: BTreeMap<String, DateTime<Utc>>,
}

/// 某一时刻的被拒记录快照。
#[derive(Debug, Default, Clone)]
pub struct RejectionStore {
    file: StoreFile,
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
            file: read_file(path),
        }
    }

    /// 该账号的被拒记录及其解封时间（可能已过期）。
    pub fn unblock(
        &self,
        account: &Account,
        quotas: &[Quota],
    ) -> Option<(&Rejection, DateTime<Utc>)> {
        let k = key(&account.provider, &account.id);
        let r = self.file.rejections.get(&k)?;
        let anchor = self.file.weekly_anchors.get(&k).copied();
        Some((r, unblock_at(r, account, quotas, anchor)))
    }

    /// 仍在封锁期内时，折算成的 `Rejected` 耗尽窗口。
    pub fn quota_for(
        &self,
        account: &Account,
        quotas: &[Quota],
        now: DateTime<Utc>,
    ) -> Option<Quota> {
        let (r, until) = self.unblock(account, quotas).filter(|(_, t)| *t > now)?;
        Some(Quota {
            provider: account.provider.clone(),
            account_id: account.id.clone(),
            window: QuotaWindow::Rejected,
            used: 100,
            limit: 100,
            reset_at: Some(until),
            status: QuotaStatus::Exhausted,
            note: r.kind.clone(),
        })
    }

    /// `quotas` 加上该账号仍在封锁期的 `Rejected` 窗口（若有）。
    pub fn with_rejection(
        &self,
        account: &Account,
        quotas: &[Quota],
        now: DateTime<Utc>,
    ) -> Vec<Quota> {
        let mut out = quotas.to_vec();
        out.extend(self.quota_for(account, quotas, now));
        out
    }

    /// 给快照里仍在封锁期的账号追加 `Rejected` 窗口；不改原快照。
    pub fn apply(&self, snapshot: &ProviderSnapshot, now: DateTime<Utc>) -> ProviderSnapshot {
        ProviderSnapshot {
            accounts: snapshot
                .accounts
                .iter()
                .map(|a| AccountWithQuotas {
                    quotas: self.with_rejection(&a.account, &a.quotas, now),
                    ..a.clone()
                })
                .collect(),
            ..snapshot.clone()
        }
    }
}

/// 解封时间，按证据强弱依次取：
///
/// 1. 上游随拒绝给出的恢复时间；
/// 2. 用户为该账号设的周重置时刻（`subswap weekly-reset`），显式指定优先于推断；
/// 3. usage 端点为该账号报出的周窗口（`7d` / 按模型 `7d`）中最早的未来重置；
/// 4. 从该账号以往带恢复时间的周限额拒绝学到的周重置，按 7 天外推；
/// 5. `auto_swap.rejection_block_ms`（默认 7 天，任何周限额都已重置）。
///
/// 2~4 取被拒之后的下一次重置；推早了代价只是一次失败请求，下一次拒绝会重新封锁。
pub fn unblock_at(
    r: &Rejection,
    account: &Account,
    quotas: &[Quota],
    weekly_anchor: Option<DateTime<Utc>>,
) -> DateTime<Utc> {
    if let Some(t) = r.reset_at {
        return t;
    }
    if let Some(w) = account.weekly_reset {
        return w.next_after(r.rejected_at);
    }
    if let Some(t) = quotas
        .iter()
        .filter(|q| matches!(q.window, QuotaWindow::SevenDay | QuotaWindow::ModelWeek))
        .filter_map(|q| q.reset_at)
        .filter(|t| *t > r.rejected_at)
        .min()
    {
        return t;
    }
    if let Some(anchor) = weekly_anchor {
        return next_weekly(anchor, r.rejected_at);
    }
    r.rejected_at + Duration::milliseconds(fallback_block_ms())
}

/// `anchor + k·7d` 中严格晚于 `after` 的最早一个。
fn next_weekly(anchor: DateTime<Utc>, after: DateTime<Utc>) -> DateTime<Utc> {
    let week = Duration::days(7).num_seconds();
    let k = (after - anchor).num_seconds().div_euclid(week) + 1;
    anchor + Duration::seconds(k * week)
}

fn fallback_block_ms() -> i64 {
    crate::settings::current()
        .auto_swap
        .rejection_block_ms
        .max(0)
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

/// 记下该账号的一次周重置时刻，供以后没有恢复时间的拒绝推算解封。
pub fn record_weekly_anchor(provider: &str, id: &AccountId, reset_at: DateTime<Utc>) -> Result<()> {
    record_weekly_anchor_at(
        &AppPaths::resolve()?.rejections_file(),
        provider,
        id,
        reset_at,
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
    update(path, |file| {
        file.rejections.insert(key(provider, id), rejection);
    })
}

pub fn record_weekly_anchor_at(
    path: &Path,
    provider: &str,
    id: &AccountId,
    reset_at: DateTime<Utc>,
) -> Result<()> {
    update(path, |file| {
        file.weekly_anchors.insert(key(provider, id), reset_at);
    })
}

pub fn clear_rejection_at(path: &Path, provider: &str, id: &AccountId) -> Result<()> {
    update(path, |file| {
        file.rejections.remove(&key(provider, id));
    })
}

fn key(provider: &str, id: &AccountId) -> String {
    format!("{provider}/{}", id.0)
}

/// 兼容 1.16.0 的扁平格式（顶层直接是 `"provider/id" → Rejection`）。
fn read_file(path: &Path) -> StoreFile {
    let Some(value) = std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
    else {
        return StoreFile::default();
    };
    let structured = value
        .as_object()
        .is_some_and(|o| o.contains_key("rejections") || o.contains_key("weekly_anchors"));
    if structured {
        serde_json::from_value(value).unwrap_or_default()
    } else {
        StoreFile {
            rejections: serde_json::from_value(value).unwrap_or_default(),
            weekly_anchors: BTreeMap::new(),
        }
    }
}

/// hook 可能与 CLI / 另一个 hook 并发写；读改写全程持独占锁。
fn update(path: &Path, f: impl FnOnce(&mut StoreFile)) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path(path))?;
    lock.lock_exclusive()?;
    let mut file = read_file(path);
    f(&mut file);
    // 剪枝不知道账号偏好与窗口，用解封时间的上界：周推算都不超过被拒后 7 天。
    let now = Utc::now();
    let max_fallback = Duration::milliseconds(fallback_block_ms()).max(Duration::days(7));
    file.rejections
        .retain(|_, r| r.reset_at.unwrap_or(r.rejected_at + max_fallback) > now);
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string_pretty(&file)?)?;
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
    use crate::model::QuotaPoolSemantics;

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn id(s: &str) -> AccountId {
        AccountId(s.into())
    }

    fn account(name: &str) -> Account {
        Account {
            provider: "claude".into(),
            id: id(name),
            label: name.into(),
            active: false,
            created_at: Utc::now(),
            last_used_at: None,
            priority: 100,
            reserve_pct: 0,
            weekly_reset: None,
            extra: serde_json::Map::new(),
        }
    }

    fn weekly(window: QuotaWindow, reset: &str) -> Quota {
        Quota {
            provider: "claude".into(),
            account_id: id("x"),
            window,
            used: 10,
            limit: 100,
            reset_at: Some(at(reset)),
            status: QuotaStatus::Ok,
            note: None,
        }
    }

    /// 2026-10-08（周四）14:00 被拒，没有上游恢复时间。
    fn unstructured() -> Rejection {
        Rejection {
            rejected_at: at("2026-10-08T14:00:00Z"),
            reset_at: None,
            kind: None,
        }
    }

    fn rejection(hours_until_reset: Option<i64>) -> Rejection {
        let now = Utc::now();
        Rejection {
            rejected_at: now,
            reset_at: hours_until_reset.map(|h| now + Duration::hours(h)),
            kind: Some("seven_day".into()),
        }
    }

    #[test]
    fn upstream_reset_wins_over_everything() {
        let mut a = account("work");
        a.weekly_reset = Some("wed 00:00".parse().unwrap());
        let r = Rejection {
            reset_at: Some(at("2026-10-09T01:00:00Z")),
            ..unstructured()
        };
        let quotas = [weekly(QuotaWindow::ModelWeek, "2026-10-11T00:00:00Z")];
        assert_eq!(
            unblock_at(&r, &a, &quotas, None),
            at("2026-10-09T01:00:00Z")
        );
    }

    #[test]
    fn manual_weekly_reset_wins_over_usage_and_anchor() {
        let mut a = account("work");
        a.weekly_reset = Some("wed 00:00".parse().unwrap());
        let quotas = [weekly(QuotaWindow::ModelWeek, "2026-10-11T00:00:00Z")];
        assert_eq!(
            unblock_at(
                &unstructured(),
                &a,
                &quotas,
                Some(at("2026-10-03T04:00:00Z"))
            ),
            at("2026-10-14T00:00:00Z")
        );
    }

    #[test]
    fn usage_weekly_window_used_when_no_manual_reset() {
        let quotas = [
            weekly(QuotaWindow::FiveHour, "2026-10-08T15:00:00Z"),
            weekly(QuotaWindow::SevenDay, "2026-10-12T04:00:00Z"),
            weekly(QuotaWindow::ModelWeek, "2026-10-11T00:00:00Z"),
            weekly(QuotaWindow::ModelWeek, "2026-10-01T00:00:00Z"),
        ];
        assert_eq!(
            unblock_at(&unstructured(), &account("work"), &quotas, None),
            at("2026-10-11T00:00:00Z")
        );
    }

    #[test]
    fn learned_anchor_projects_forward_in_weeks() {
        assert_eq!(
            unblock_at(
                &unstructured(),
                &account("work"),
                &[],
                Some(at("2026-09-26T04:00:00Z"))
            ),
            at("2026-10-10T04:00:00Z")
        );
        assert_eq!(
            next_weekly(at("2026-10-10T04:00:00Z"), at("2026-10-08T14:00:00Z")),
            at("2026-10-10T04:00:00Z")
        );
        assert_eq!(
            next_weekly(at("2026-10-10T04:00:00Z"), at("2026-10-10T04:00:00Z")),
            at("2026-10-17T04:00:00Z")
        );
    }

    #[test]
    fn falls_back_to_block_duration() {
        let r = unstructured();
        assert_eq!(
            unblock_at(&r, &account("work"), &[], None),
            r.rejected_at + Duration::milliseconds(fallback_block_ms())
        );
    }

    #[test]
    fn recorded_rejection_becomes_exhausted_window_until_reset() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rejections.json");
        record_rejection_at(&path, "claude", &id("work"), rejection(Some(30))).unwrap();

        let store = RejectionStore::load_from(&path);
        let now = Utc::now();
        let q = store.quota_for(&account("work"), &[], now).unwrap();
        assert_eq!(q.window, QuotaWindow::Rejected);
        assert_eq!(q.status, QuotaStatus::Exhausted);
        assert_eq!(q.note.as_deref(), Some("seven_day"));
        assert!(store.quota_for(&account("other"), &[], now).is_none());
        assert!(store
            .quota_for(&account("work"), &[], now + Duration::hours(31))
            .is_none());
    }

    #[test]
    fn stored_anchor_drives_unstructured_rejection() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rejections.json");
        let now = Utc::now();
        record_weekly_anchor_at(&path, "claude", &id("work"), now - Duration::days(5)).unwrap();
        record_rejection_at(
            &path,
            "claude",
            &id("work"),
            Rejection {
                rejected_at: now,
                reset_at: None,
                kind: None,
            },
        )
        .unwrap();
        let q = RejectionStore::load_from(&path)
            .quota_for(&account("work"), &[], now)
            .unwrap();
        assert_eq!(q.reset_at, Some(now + Duration::days(2)));
    }

    #[test]
    fn reads_flat_file_written_by_1_16_0() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rejections.json");
        let reset = Utc::now() + Duration::hours(30);
        std::fs::write(
            &path,
            serde_json::json!({
                "claude/work": {"rejected_at": Utc::now(), "reset_at": reset, "kind": "seven_day"}
            })
            .to_string(),
        )
        .unwrap();
        let store = RejectionStore::load_from(&path);
        assert!(store.quota_for(&account("work"), &[], Utc::now()).is_some());
        record_weekly_anchor_at(&path, "claude", &id("work"), reset).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("\"rejections\"") && raw.contains("claude/work"),
            "{raw}"
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
            .quota_for(&account("a"), &[], Utc::now())
            .is_none());
    }

    #[test]
    fn corrupt_file_reads_as_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rejections.json");
        std::fs::write(&path, "{not json").unwrap();
        assert!(RejectionStore::load_from(&path)
            .quota_for(&account("a"), &[], Utc::now())
            .is_none());
    }

    #[test]
    fn apply_appends_window_only_to_rejected_account() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rejections.json");
        record_rejection_at(&path, "claude", &id("work"), rejection(Some(30))).unwrap();
        let mk = |name: &str| AccountWithQuotas {
            account: account(name),
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

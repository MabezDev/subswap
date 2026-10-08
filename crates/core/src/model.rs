//! subswap 统一数据模型。
//!
//! 这里只放语义清晰、Provider 共通的字段；Provider 私有细节放各自实现里。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

/// 账号 ID。Provider 内部唯一，全局组合 `(provider_id, account_id)` 才唯一。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AccountId(pub String);

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for AccountId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

/// 账号元数据。凭证本身不在这里，由 [`crate::store::CredentialStore`] 持有。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub provider: String,
    pub id: AccountId,
    /// 用户友好标签，例如邮箱前缀或备注。
    pub label: String,
    /// 是否为当前激活账号。
    pub active: bool,
    /// 创建/导入时间。
    pub created_at: DateTime<Utc>,
    /// 上次成功使用时间（切换或调用）。
    pub last_used_at: Option<DateTime<Utc>>,
    /// 用户给的优先级（数字越小越优先，默认 100）。自动切换挑候选时先比它；
    /// 当前账号健康时也会切回更优先且余量充足的账号（见 `auto_policy`）。
    /// 只由 `subswap priority` 修改，[`crate::AccountRegistry::upsert`] 会保留已有值。
    #[serde(default = "default_priority")]
    pub priority: i32,
    /// 每个窗口留给 subswap 之外（手机、其他机器）的余量百分比，默认 0。
    /// 自动切换把用量达到 `100 - reserve_pct` 的窗口当作耗尽；手动 `swap` 不受影响。
    /// 只由 `subswap reserve` 修改，[`crate::AccountRegistry::upsert`] 会保留已有值。
    #[serde(default, skip_serializing_if = "is_zero")]
    pub reserve_pct: u8,
    /// 用户指定的周额度重置时刻（UTC）。客户端被拒且上游没给恢复时间时，按它推算解封时间；
    /// 只由 `subswap weekly-reset` 修改，[`crate::AccountRegistry::upsert`] 会保留已有值。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly_reset: Option<WeeklyReset>,
    /// 任意 Provider 私有 KV，用于扩展（不入 keyring）。
    #[serde(default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Account {
    /// 是否只能由用户手动激活。此类账号不会触发自动切出，也不会成为自动切换候选。
    pub fn manual_only(&self) -> bool {
        self.extra
            .get("manual_only")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    }

    /// 计费方式。读 `extra["billing"]`，缺省（原生登录的订阅账号）视为 [`BillingKind::Flat`]。
    ///
    /// 这是给下游消费者（如 OpenConductor）判断"按量花钱"的唯一信号；新增 Provider
    /// 适配器（公司号、不限量 APIKEY、第三方中转号池等）只需在 `extra` 里如实标注，
    /// 不需要 subswap-core 认识具体账号名。
    ///
    /// 向后兼容：早于 0.3.23 版本登记的 API 账号没有 `billing` 字段，
    /// 但会带 `kind=api`（见 `subswap add-api` 的历史写入），自动视为 metered。
    pub fn billing(&self) -> BillingKind {
        if let Some(billing) = self
            .extra
            .get("billing")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| s.parse().ok())
        {
            return billing;
        }
        // 旧账号：kind=api 暗示按量计费端点（自定义 API 节点默认按量）。
        if self.extra.get("kind").and_then(serde_json::Value::as_str) == Some("api") {
            return BillingKind::Metered;
        }
        BillingKind::Flat
    }
}

fn default_priority() -> i32 {
    100
}

fn is_zero(v: &u8) -> bool {
    *v == 0
}

/// 每周固定的重置时刻（UTC），如 `Sun 00:00`。序列化为该字符串。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct WeeklyReset {
    pub weekday: chrono::Weekday,
    pub hour: u32,
    pub minute: u32,
}

impl WeeklyReset {
    /// 严格晚于 `after` 的下一次重置时刻。
    pub fn next_after(&self, after: DateTime<Utc>) -> DateTime<Utc> {
        use chrono::{Datelike, Duration, TimeZone};
        let days_ahead = (i64::from(self.weekday.num_days_from_monday())
            - i64::from(after.weekday().num_days_from_monday()))
        .rem_euclid(7);
        let date = after.date_naive() + Duration::days(days_ahead);
        let candidate = Utc.from_utc_datetime(
            &date
                .and_hms_opt(self.hour, self.minute, 0)
                .expect("hour and minute validated on construction"),
        );
        if candidate > after {
            candidate
        } else {
            candidate + Duration::days(7)
        }
    }
}

impl fmt::Display for WeeklyReset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {:02}:{:02}", self.weekday, self.hour, self.minute)
    }
}

impl std::str::FromStr for WeeklyReset {
    type Err = String;

    /// 接受 `sun`、`Sunday 05:30` 等：星期必填，时刻可省（默认 00:00）。
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let mut parts = s.split_whitespace();
        let weekday: chrono::Weekday = parts
            .next()
            .ok_or("expected a weekday, e.g. `sun` or `sun 04:00`")?
            .parse()
            .map_err(|_| format!("unknown weekday in {s:?}"))?;
        let (hour, minute) = match parts.next() {
            None => (0, 0),
            Some(t) => {
                let (h, m) = t
                    .split_once(':')
                    .ok_or_else(|| format!("expected HH:MM, got {t:?}"))?;
                let hour: u32 = h.parse().map_err(|_| format!("bad hour in {t:?}"))?;
                let minute: u32 = m.parse().map_err(|_| format!("bad minute in {t:?}"))?;
                if hour > 23 || minute > 59 {
                    return Err(format!("time out of range: {t:?}"));
                }
                (hour, minute)
            }
        };
        if parts.next().is_some() {
            return Err(format!("unexpected trailing text in {s:?}"));
        }
        Ok(Self {
            weekday,
            hour,
            minute,
        })
    }
}

impl TryFrom<String> for WeeklyReset {
    type Error = String;

    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<WeeklyReset> for String {
    fn from(value: WeeklyReset) -> Self {
        value.to_string()
    }
}

/// 账号的计费方式：决定它在自动切换中的优先级与对外的"是否真花钱"语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BillingKind {
    /// 固定费率订阅（如官方登录号），用量在套餐内不额外计费。
    Flat,
    /// 按量计费（如自定义 API 端点接的按 token 计费上游）。
    Metered,
    /// 不限量（如公司自建网关、不限量 API Key）。
    Unlimited,
}

impl fmt::Display for BillingKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Flat => "flat",
            Self::Metered => "metered",
            Self::Unlimited => "unlimited",
        };
        f.write_str(s)
    }
}

impl std::str::FromStr for BillingKind {
    type Err = ();

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "flat" => Ok(Self::Flat),
            "metered" => Ok(Self::Metered),
            "unlimited" => Ok(Self::Unlimited),
            _ => Err(()),
        }
    }
}

/// 额度统计窗口。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaWindow {
    /// Claude 的 5 小时窗口。
    FiveHour,
    /// Claude 的 7 天窗口。
    SevenDay,
    /// 月度窗口（Codex 等）。
    Month,
    /// Cursor 官方模型用量。
    FirstPartyModels,
    /// Cursor API 用量。
    Api,
    /// Cursor 套餐 Credits（美元账本；`used`/`limit` 存分）。
    Credits,
    /// 按模型（或产品面）单列的周额度，如 Claude Team 的 Fable 周上限。
    /// `note` 存模型显示名；与 `SevenDay` 同样只在耗尽时阻断。
    ModelWeek,
    /// 原生客户端的真实请求被额度拒绝（见 `rejections`）。恒为耗尽，`reset_at` 是封锁截止，
    /// `note` 存上游限额类型。只在决策 / 展示时叠加，不进 quota 缓存。
    Rejected,
    /// Codex 限额重置道具（banked reset）：`used` = 可用数，`limit` = 0（不参与百分比与自动切换判定），
    /// `reset_at` = 最早过期时间。`0` 时不产生该窗口。
    ResetCredits,
    /// 其他自定义窗口。
    Custom,
}

/// 额度状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaStatus {
    /// 健康可用。
    Ok,
    /// 接近展示阈值。
    Warn,
    /// 已耗尽或被限流。
    Exhausted,
    /// 查询失败 / 未知。
    Unknown,
}

impl QuotaStatus {
    /// 用统一阈值（[`crate::settings::Quota::warn_pct`] / `exhausted_pct`）
    /// 把已用百分比（0~100）映射为 [`QuotaStatus`]。
    ///
    /// 各 Provider 的 `query_quota` 不要自己写阈值分支，统一走这里，
    /// 调阈值只改 `config.toml` 一处。
    pub fn from_percent(pct: f64) -> Self {
        let q = crate::settings::current().quota.clone();
        if pct >= q.exhausted_pct {
            Self::Exhausted
        } else if pct >= q.warn_pct {
            Self::Warn
        } else {
            Self::Ok
        }
    }
}

/// 额度池语义：决定自动切换如何判定“可用”与“恢复”。
/// 由各 Provider 通过 [`crate::Provider::quota_pool_semantics`] 声明，
/// 共享决策逻辑只读该声明，不按 Provider 名分发。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaPoolSemantics {
    /// 叠加池（默认）：大窗口包含小窗口，任一窗口耗尽整体即不可用；
    /// 恢复取阻塞窗口中最晚者。
    Stacked,
    /// 并行池：任一池有余量即可承接，仅全部耗尽才需切换；
    /// 恢复取最早者。
    Parallel,
}

/// 单个窗口的额度快照。一个账号可能同时存在多个窗口（如 Claude 同时给 5h 与 7d）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Quota {
    pub provider: String,
    pub account_id: AccountId,
    pub window: QuotaWindow,
    /// 已使用量（单位由 Provider 自行约定，通常是 tokens 或百分比基数）。
    pub used: u64,
    /// 上限。0 表示未知。
    pub limit: u64,
    /// 重置时间（若 Provider 提供）。
    pub reset_at: Option<DateTime<Utc>>,
    pub status: QuotaStatus,
    /// 人类可读补充说明（错误信息、提示等）。
    #[serde(default)]
    pub note: Option<String>,
}

impl Quota {
    /// 使用率 0.0~1.0；limit=0 时返回 None。
    pub fn usage_ratio(&self) -> Option<f64> {
        if self.limit == 0 {
            None
        } else {
            Some(self.used as f64 / self.limit as f64)
        }
    }

    /// 是否达到给定阈值（0.0~1.0）。limit 未知时返回 false（保守不触发自动切换）。
    pub fn is_above(&self, threshold: f64) -> bool {
        self.usage_ratio().map(|r| r >= threshold).unwrap_or(false)
    }
}

/// `rm` 断开原生凭证的结果。`Err` 表示原生没断掉，调用方必须直接报错退出、
/// 不清本地记录，避免“删了又回来”的假成功。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfficialDisconnect {
    /// 原生凭证已断开/登出，后续同步不会导回。
    Disconnected,
    /// 原生本来就没有这份凭证（parked，或已在别处登出），直接清本地即可（幂等）。
    AlreadyGone,
    /// 原生不支持自动断（如 V1 Console），保持只清本地 + 旧提示。
    Unsupported,
}

/// 一次切换可能要触达的本地客户端目标（CLI、IDE 扩展、桌面端等）。
/// Provider 在 `client_targets()` 中声明，切换时由统一的 FileSyncer 处理。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientTarget {
    /// 客户端标识，例如 `codex_cli` / `codex_vscode` / `claude_cli`。
    pub id: String,
    /// 人类可读名称，用于 doctor / 日志输出。
    pub display_name: String,
    /// 该客户端的根目录或主配置文件，doctor 用来探测是否存在。
    pub probe_path: std::path::PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn weekly_reset_parses_and_round_trips() {
        let r: WeeklyReset = "sunday 04:30".parse().unwrap();
        assert_eq!(r.to_string(), "Sun 04:30");
        assert_eq!(
            "sun".parse::<WeeklyReset>().unwrap().to_string(),
            "Sun 00:00"
        );
        for bad in ["", "funday", "sun 24:00", "sun 4", "sun 04:00 extra"] {
            assert!(bad.parse::<WeeklyReset>().is_err(), "{bad:?}");
        }
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(json, "\"Sun 04:30\"");
        assert_eq!(serde_json::from_str::<WeeklyReset>(&json).unwrap(), r);
    }

    #[test]
    fn weekly_reset_next_after_is_strictly_later_and_within_a_week() {
        let sun_midnight: WeeklyReset = "sun 00:00".parse().unwrap();
        // 2026-10-08 是周四。
        assert_eq!(
            sun_midnight.next_after(at("2026-10-08T14:00:00Z")),
            at("2026-10-11T00:00:00Z")
        );
        // 恰好在重置时刻：取下一周。
        assert_eq!(
            sun_midnight.next_after(at("2026-10-11T00:00:00Z")),
            at("2026-10-18T00:00:00Z")
        );
        let thu_late: WeeklyReset = "thu 20:00".parse().unwrap();
        assert_eq!(
            thu_late.next_after(at("2026-10-08T14:00:00Z")),
            at("2026-10-08T20:00:00Z")
        );
    }
}

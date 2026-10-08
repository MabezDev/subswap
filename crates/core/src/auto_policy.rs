//! 自动切换策略：给定一个 Provider 的账号 + 额度快照，决定要不要切、切到谁。
//!
//! 设计要点：
//! - **纯函数**：不读环境、不发网络、不写文件。所有 IO 在调用方完成；这里只决策。
//! - 不跨 Provider 决策（自动切换默认不跨 Provider；用户可能并非两边都付费）。
//! - 决策结果显式区分 [`PolicyDecision::NoOp`] / [`PolicyDecision::Swap`] / [`PolicyDecision::Degraded`]，
//!   `Degraded` 是显式终态：调用方必须提示用户手动 `subswap swap`，不能盲切。
//!
//! 规则细节见 docs/design/AUTO_SWAP_DESIGN.md。

use chrono::{DateTime, Utc};

use crate::model::{Account, AccountId, Quota, QuotaPoolSemantics, QuotaStatus, QuotaWindow};
use crate::settings;

#[derive(Debug, Clone, Copy)]
pub struct PolicyConfig {
    /// 自动切换总开关。false 时 `decide()` 立即返回 `NoOp`。
    pub enabled: bool,
    /// 触发阈值，0.0~1.0。默认值来自当前生效的配置（`config.toml > auto_swap.threshold`）。
    pub threshold: f64,
    /// 是否允许把 status=Unknown 的账号作为候选。默认 false（保守）。
    pub allow_unknown: bool,
    /// 兼容旧配置的沉淀宽限期（毫秒）。不确定额度现已始终禁止自动切走，
    /// 此字段保留给已有调用方，不再改变决策。
    pub settle_grace_ms: i64,
    /// 手动切换保持期（毫秒）。用户手动 `swap` / `login` 后，该 provider 在此窗口内
    /// 暂停一切自动切换（连确定性额度切换一起挡），避免把显式选择掰回去。
    /// `hold_remaining_ms()` 为 fail-open 文件态；`0` 或负数关闭。
    pub manual_hold_ms: i64,
    /// 回切偏好账号的余量门槛，0.0~1.0（`config.toml > auto_swap.return_threshold`）。
    pub return_threshold: f64,
}

/// 测试专用构造：显式字段 + 保持关闭，避免 `SUBSWAP_HOME` 环境互相干扰、
/// 某个用例写的保持文件污染其他用例的 `PolicyConfig::default()`。
/// 生产路径一律用 `PolicyConfig::default()`（读全局 settings）。
#[cfg(test)]
fn test_config(settle_grace_ms: i64) -> PolicyConfig {
    PolicyConfig {
        enabled: true,
        threshold: 0.98,
        allow_unknown: false,
        settle_grace_ms,
        manual_hold_ms: 0,
        return_threshold: 0.90,
    }
}

impl Default for PolicyConfig {
    fn default() -> Self {
        let s = settings::current();
        Self {
            enabled: s.auto_swap.enabled,
            threshold: s.auto_swap.threshold,
            allow_unknown: false,
            settle_grace_ms: s.auto_swap.settle_grace_ms,
            manual_hold_ms: s.auto_swap.manual_hold_ms,
            return_threshold: s.auto_swap.return_threshold,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AccountWithQuotas {
    pub account: Account,
    pub quotas: Vec<Quota>,
    /// 拉取状态。CLI 渐进刷新时可能把 [`QuotaFetchState::Loading`] 传入决策；
    /// 当前账号未完成查询时不切换；候选也必须有已完成的可用额度。
    pub fetch_state: QuotaFetchState,
}

/// 单次 `query_quota` 的状态机。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum QuotaFetchState {
    /// CLI 首屏渲染骨架时的占位；尚未发起或尚未返回。
    Loading,
    /// 拉取完成（`quotas` 是结果，允许为空）。
    #[default]
    Ready,
    /// 拉取失败，附带错误描述。
    Failed(String),
    /// 实时查询失败，但存在未过期的缓存数据（由 `QuotaCache` 回填）。
    /// 对应账号的 `AccountWithQuotas.quotas` 存放缓存快照。
    /// 缓存仅用于展示，不能成为自动切换的触发或候选依据。
    Stale {
        cached_at: chrono::DateTime<chrono::Utc>,
        error: String,
    },
}

impl QuotaFetchState {
    /// 拉取失败（且无可用缓存）时返回错误文本；其他状态返回 `None`。
    pub fn failed(&self) -> Option<&str> {
        match self {
            Self::Failed(e) => Some(e.as_str()),
            _ => None,
        }
    }

    pub fn is_loading(&self) -> bool {
        matches!(self, Self::Loading)
    }
}

#[derive(Debug, Clone)]
pub struct ProviderSnapshot {
    pub provider: String,
    pub accounts: Vec<AccountWithQuotas>,
    /// 该 provider 声明的额度池语义（见 [`QuotaPoolSemantics`]）。
    /// 构建快照时从 provider 对象取（`Provider::quota_pool_semantics`），
    /// 决策逻辑只读该字段，不按 provider 名分发。
    pub pool_semantics: QuotaPoolSemantics,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    /// 当前激活账号还在阈值内，不动。
    NoOp { reason: String },
    /// 决定切换。`from` 为空表示当前没有激活账号。
    Swap {
        from: Option<AccountId>,
        to: AccountId,
        reason: String,
    },
    /// 自动切换不可用，需要人工介入（手动 swap）。
    Degraded { reason: String },
}

pub fn decide(snapshot: &ProviderSnapshot, config: &PolicyConfig) -> PolicyDecision {
    if !config.enabled {
        return PolicyDecision::NoOp {
            reason: "auto swap disabled".into(),
        };
    }
    let reserved = ProviderSnapshot {
        accounts: snapshot
            .accounts
            .iter()
            .map(|a| AccountWithQuotas {
                quotas: reserve_adjusted_quotas(&a.account, &a.quotas),
                ..a.clone()
            })
            .collect(),
        ..snapshot.clone()
    };
    let snapshot = &reserved;
    if snapshot.accounts.is_empty() {
        return PolicyDecision::Degraded {
            reason: format!("provider {} has no accounts", snapshot.provider),
        };
    }

    let active = snapshot.accounts.iter().find(|a| a.account.active);
    let active_id = active.map(|a| a.account.id.clone());

    // manual_only 账号只允许用户显式切换。激活后自动切换完全停用。
    if let Some(active) = active.filter(|active| active.account.manual_only()) {
        return PolicyDecision::NoOp {
            reason: format!("{} is manual-only", active.account.id),
        };
    }

    // 手动保持：用户刚手动切换过该 provider 时，整个 provider 暂停自动切换
    // （连确定性额度切换一起挡），把显式选择留给用户。fail-open：文件缺失/损坏视为无保持。
    // 例外：当前账号的真实请求已被拒（`Rejected`），手动选择已经不可用，保持只会让用户卡住。
    let active_rejected = active.is_some_and(|a| {
        a.quotas
            .iter()
            .any(|q| matches!(q.window, QuotaWindow::Rejected))
    });
    if config.manual_hold_ms > 0 && !active_rejected {
        let remaining = crate::manual_hold::hold_remaining_ms(&snapshot.provider);
        if remaining > 0 {
            let active_name = active
                .map(|a| a.account.id.to_string())
                .unwrap_or_else(|| "-".into());
            return PolicyDecision::NoOp {
                reason: format!(
                    "{active_name} manually selected; auto swap held for {}s",
                    remaining.max(1000) / 1000,
                ),
            };
        }
    }

    // 1. 查询未完成或失败只能说明额度未知，不能说明当前账号不可用。
    // 候选先返回、缓存过期、额度端点 429 都不能改变当前会话的账号。
    // 被拒窗口是客户端真实请求的拒绝，本身就是确认，不依赖 usage 查询结果。
    if let Some(a) = active.filter(|_| !active_rejected) {
        match &a.fetch_state {
            QuotaFetchState::Loading => {
                return PolicyDecision::NoOp {
                    reason: format!("{} quota still loading; keep current account", a.account.id),
                };
            }
            QuotaFetchState::Failed(error) | QuotaFetchState::Stale { error, .. } => {
                return PolicyDecision::Degraded {
                    reason: format!(
                        "active account {} quota fetch failed ({}); cannot decide",
                        a.account.id, error
                    ),
                };
            }
            QuotaFetchState::Ready => {}
        }
    }

    // 2. 判断当前 active 是否需要切走。
    let needs_swap = match active {
        Some(a) => account_needs_swap(a, snapshot.pool_semantics, config.threshold),
        None => true, // 没有 active 时主动选一个激活
    };

    if !needs_swap {
        if let Some((a, target)) =
            active.and_then(|a| preferred_return_target(snapshot, a, config).map(|t| (a, t)))
        {
            return PolicyDecision::Swap {
                from: active_id,
                to: target.account.id.clone(),
                reason: format!(
                    "{} within threshold; return to preferred {}",
                    a.account.id, target.account.id
                ),
            };
        }
        let id = active.map(|a| a.account.id.to_string()).unwrap_or_default();
        return PolicyDecision::NoOp {
            reason: format!("{} within threshold", id),
        };
    }

    // 3. 筛候选：排除当前激活，优先选择当前可承接流量的账号。
    let candidates: Vec<&AccountWithQuotas> = snapshot
        .accounts
        .iter()
        .filter(|a| Some(&a.account.id) != active_id.as_ref())
        .filter(|a| !a.account.manual_only())
        .filter(|a| {
            is_viable_candidate(
                a,
                snapshot.pool_semantics,
                config.threshold,
                config.allow_unknown,
            )
        })
        .collect();

    if let Some(best) = candidates
        .into_iter()
        .min_by(|a, b| compare_candidates(a, b))
    {
        let reason = match active {
            Some(a) => format!(
                "{} above {:.0}% threshold; pick {} (most headroom)",
                a.account.id,
                config.threshold * 100.0,
                best.account.id
            ),
            None => format!("no active account; activate {}", best.account.id),
        };

        return PolicyDecision::Swap {
            from: active_id,
            to: best.account.id.clone(),
            reason,
        };
    }

    // 没有已确认可用的目标：先试全员耗尽回退（切到确认恢复最快的耗尽号），
    // 实在没有可比的才 Degraded。查询失败 / 未知账号永不成为目标。
    match fallback_to_soonest_recovery(snapshot, active_id.as_ref(), config.threshold) {
        Fallback::SwapTo(to) => {
            let reason = match active {
                Some(a) => format!(
                    "{} exhausted and no usable target; pick {} (all exhausted, recovers soonest)",
                    a.account.id, to,
                ),
                None => {
                    format!("no active account and none usable; activate {to} (recovers soonest)")
                }
            };
            return PolicyDecision::Swap {
                from: active_id,
                to,
                reason,
            };
        }
        Fallback::StayCurrent => {
            let id = active.map(|a| a.account.id.to_string()).unwrap_or_default();
            return PolicyDecision::NoOp {
                reason: format!("{id} all exhausted but current recovers soonest"),
            };
        }
        Fallback::NoPool => {}
    }
    PolicyDecision::Degraded {
        reason: "no swap candidate (others exhausted / fetch failed / unknown status)".into(),
    }
}

/// 按账号 `reserve_pct` 折算策略视角的额度：用量达到 `100 - reserve_pct` 的窗口记为 `Exhausted`。
///
/// 折算后切走、候选、全员耗尽回退都把保留余量当作已耗尽，自动切换不会花掉它；
/// 回退时宁可切到恢复更早的耗尽号，也不留在保留区里继续用。只影响决策与展示着色，
/// 不改写缓存或 `--json` 输出的原始用量。
pub fn reserve_adjusted_quotas(account: &Account, quotas: &[Quota]) -> Vec<Quota> {
    let mut out = quotas.to_vec();
    let Some(line) = reserve_line(account) else {
        return out;
    };
    for q in &mut out {
        if quota_gates_auto_swap(q)
            && q.limit > 0
            && !matches!(q.status, QuotaStatus::Unknown)
            && q.is_above(line)
        {
            q.status = QuotaStatus::Exhausted;
        }
    }
    out
}

/// 保留余量对应的用量线（0.0~1.0）；未设置保留时为 `None`。
fn reserve_line(account: &Account) -> Option<f64> {
    (account.reserve_pct > 0).then(|| 1.0 - f64::from(account.reserve_pct.min(100)) / 100.0)
}

/// 当前账号健康时，找一个 `priority` 严格更优、已确认可用且余量明显充足的账号切回。
///
/// 小时级窗口须低于 [`return_line`]：它会在会话中途恢复，离触发线太近会刚切回就切走。
/// 长窗口只增不减，可用（未达阻断线）即可，否则周额度里阻断线以下的那段会被白白闲置。
/// 未知 / 无窗口账号永不作为回切目标。
fn preferred_return_target<'a>(
    snapshot: &'a ProviderSnapshot,
    active: &AccountWithQuotas,
    config: &PolicyConfig,
) -> Option<&'a AccountWithQuotas> {
    snapshot
        .accounts
        .iter()
        .filter(|a| a.account.id != active.account.id)
        .filter(|a| a.account.priority < active.account.priority)
        .filter(|a| is_viable_candidate(a, snapshot.pool_semantics, config.threshold, false))
        .filter(|a| has_return_headroom(a, return_line(&a.account, config)))
        .min_by(|a, b| compare_candidates(a, b))
}

/// 小时级窗口的回切线：与该账号实际切走线保持 `threshold - return_threshold` 的滞回间隔。
/// 设了保留余量时切走线下移到 `1 - reserve`，回切线随之下移。
fn return_line(account: &Account, config: &PolicyConfig) -> f64 {
    let gap = (config.threshold - config.return_threshold).max(0.0);
    let swap_line = reserve_line(account).map_or(config.threshold, |l| l.min(config.threshold));
    config.return_threshold.min(swap_line - gap)
}

fn has_return_headroom(a: &AccountWithQuotas, line: f64) -> bool {
    let quotas: Vec<&Quota> = auto_swap_quotas(&a.quotas).collect();
    !quotas.is_empty()
        && quotas.iter().all(|q| {
            q.limit > 0
                && !matches!(q.status, QuotaStatus::Unknown)
                && !(matches!(q.window, QuotaWindow::FiveHour) && q.is_above(line))
        })
}

/// 全员耗尽回退的结果：切走 / 留守（当前恢复最快）/ 无池可比。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Fallback {
    SwapTo(AccountId),
    StayCurrent,
    NoPool,
}

/// 全员耗尽回退（2026-09-30 用户决策，全 provider 通用）：无可用候选时，
/// 在已确认死亡的账号里选恢复最快的一个，而不是守着恢复最慢的当前号。
///
/// 触发门槛：当前号已确认死亡（至少一个 gating 窗口 `Exhausted` 且 limit > 0），
/// 或没有当前号。仅 Warn / 小时级超阈值（仍可服务）的当前号不动。
///
/// 入池（缺一不可）：非当前号、非 `manual_only`、`Ready`、全部 gating 窗口已确认
/// （`limit > 0` 且状态已知）、自身已不可用（`account_needs_swap`）、有效恢复时间已知
/// （见 `effective_recovery`）。失败 / 加载中 / 缓存 / 未知账号永不入池。
/// 按有效恢复时间排序（并列按 priority、账号 ID）：优胜者恢复严格早于当前号
/// （或当前恢复时间未知 / 无当前号）→ [`Fallback::SwapTo`]；池非空但当前号恢复
/// 最快（或并列）→ [`Fallback::StayCurrent`]；池为空 → [`Fallback::NoPool`]。
fn fallback_to_soonest_recovery(
    snapshot: &ProviderSnapshot,
    active_id: Option<&AccountId>,
    threshold: f64,
) -> Fallback {
    let active = active_id.and_then(|id| snapshot.accounts.iter().find(|a| a.account.id == *id));
    if let Some(a) = active {
        let confirmed_dead = auto_swap_quotas(&a.quotas)
            .any(|q| q.limit > 0 && matches!(q.status, QuotaStatus::Exhausted));
        if !confirmed_dead {
            return Fallback::NoPool;
        }
    }
    let active_recovery =
        active.and_then(|a| effective_recovery(a, snapshot.pool_semantics, threshold));
    let mut pool: Vec<(&AccountWithQuotas, DateTime<Utc>)> = snapshot
        .accounts
        .iter()
        .filter(|a| Some(&a.account.id) != active_id)
        .filter(|a| !a.account.manual_only())
        .filter(|a| matches!(a.fetch_state, QuotaFetchState::Ready))
        .filter(|a| {
            let quotas: Vec<&Quota> = auto_swap_quotas(&a.quotas).collect();
            !quotas.is_empty()
                && quotas
                    .iter()
                    .all(|q| q.limit > 0 && !matches!(q.status, QuotaStatus::Unknown))
        })
        .filter(|a| account_needs_swap(a, snapshot.pool_semantics, threshold))
        .filter_map(|a| {
            effective_recovery(a, snapshot.pool_semantics, threshold).map(|reset| (a, reset))
        })
        .collect();
    pool.sort_by(|(a, reset_a), (b, reset_b)| {
        reset_a
            .cmp(reset_b)
            .then(a.account.priority.cmp(&b.account.priority))
            .then(a.account.id.0.cmp(&b.account.id.0))
    });
    let (winner, winner_reset) = match pool.into_iter().next() {
        Some(first) => first,
        None => return Fallback::NoPool,
    };
    let sooner = match active_recovery {
        Some(current) => winner_reset < current,
        // 当前恢复时间未知 / 无当前号：有明确恢复时间的候选总比没有强。
        None => true,
    };
    if sooner {
        Fallback::SwapTo(winner.account.id.clone())
    } else {
        Fallback::StayCurrent
    }
}

/// 阻塞窗口：`Exhausted`（`limit > 0`）或 `FiveHour` 超阈值。
/// 叠加池与 [`account_needs_swap`] 共用此定义，避免两处各写一遍谓词。
fn is_blocking(q: &Quota, threshold: f64) -> bool {
    (q.limit > 0 && matches!(q.status, QuotaStatus::Exhausted))
        || quota_exceeds_auto_threshold(q, threshold)
}

/// 有效恢复时间：账号从“不可用”回到“可用”的预计时间。
///
/// 叠加池：大窗口包含小窗口，任一阻塞未恢复整体仍不可用，取阻塞中最晚的 `reset_at`；
/// 任一阻塞缺 `reset_at` 则未知。并行池：任一池恢复即恢复，取最早的已知 `reset_at`。
fn effective_recovery(
    a: &AccountWithQuotas,
    semantics: QuotaPoolSemantics,
    threshold: f64,
) -> Option<DateTime<Utc>> {
    let quotas: Vec<&Quota> = auto_swap_quotas(&a.quotas).collect();
    if quotas.is_empty() {
        return None;
    }
    if parallel_pools(semantics, &quotas) {
        return quotas.iter().filter_map(|q| q.reset_at).min();
    }
    let mut blocking = quotas.into_iter().filter(|q| is_blocking(q, threshold));
    let mut latest = blocking.next()?.reset_at?;
    for q in blocking {
        latest = latest.max(q.reset_at?);
    }
    Some(latest)
}

fn account_needs_swap(
    a: &AccountWithQuotas,
    semantics: QuotaPoolSemantics,
    threshold: f64,
) -> bool {
    if a.quotas.is_empty() {
        return false; // 无窗口数据时不主动切（保守）
    }
    let quotas: Vec<&Quota> = auto_swap_quotas(&a.quotas).collect();
    if quotas.is_empty() {
        return false;
    }
    // 并行池：任一池仍可用就不必切；全部耗尽才切。
    if parallel_pools(semantics, &quotas) {
        let fivehour_over = quotas
            .iter()
            .any(|q| quota_exceeds_auto_threshold(q, threshold));
        let all_exhausted = quotas
            .iter()
            .all(|q| q.limit > 0 && matches!(q.status, QuotaStatus::Exhausted));
        return fivehour_over || all_exhausted;
    }
    // 叠加池：任一阻塞（耗尽或小时级超阈值）即切，定义见 `is_blocking`。
    quotas.iter().any(|q| is_blocking(q, threshold))
}

fn is_viable_candidate(
    a: &AccountWithQuotas,
    semantics: QuotaPoolSemantics,
    threshold: f64,
    allow_unknown: bool,
) -> bool {
    if a.account.manual_only() {
        return false;
    }
    if !matches!(a.fetch_state, QuotaFetchState::Ready) {
        return false;
    }
    if a.quotas.is_empty() {
        return allow_unknown;
    }
    // 候选不能有小时级窗口达到/超过 threshold，否则切过去仍无法正常承接流量。
    // 长窗口只在明确 Exhausted 时阻断。并行池走“任一可用即可”（见 `parallel_pools`）。
    let quotas: Vec<&Quota> = auto_swap_quotas(&a.quotas).collect();
    if quotas.is_empty() {
        return allow_unknown;
    }
    let no_above_threshold = quotas
        .iter()
        .all(|q| !quota_exceeds_auto_threshold(q, threshold));
    if parallel_pools(semantics, &quotas) {
        let any_usable = quotas
            .iter()
            .any(|q| q.limit > 0 && matches!(q.status, QuotaStatus::Ok | QuotaStatus::Warn));
        return if allow_unknown {
            no_above_threshold
        } else {
            no_above_threshold && any_usable
        };
    }
    let no_exhausted = quotas
        .iter()
        .all(|q| !matches!(q.status, QuotaStatus::Exhausted));
    if allow_unknown {
        no_above_threshold && no_exhausted
    } else {
        // Warn 只是展示着色（`quota.warn_pct` 不参与决策）。未知窗口不能当可用证据。
        let any_usable = quotas
            .iter()
            .any(|q| q.limit > 0 && matches!(q.status, QuotaStatus::Ok | QuotaStatus::Warn));
        any_usable && no_above_threshold && no_exhausted
    }
}

fn quota_exceeds_auto_threshold(q: &Quota, threshold: f64) -> bool {
    matches!(q.window, QuotaWindow::FiveHour)
        && !matches!(q.status, QuotaStatus::Unknown)
        && q.is_above(threshold)
}

/// 自动切换参与判定的窗口。
///
/// 并行池的 `1st` / Credits / `API` 全部参与，靠 `parallel_pools` 做「任一可用即可」；
/// 不再排除 `API`（否则全员 1st 见底时会退化成只按重置时间挑全空号）。
/// 叠加池（Claude 5h/7d、Codex 月度等）全部参与且任一耗尽即切。
/// Codex 重置道具（`ResetCredits`）只读展示，永不参与判定。
fn quota_gates_auto_swap(q: &Quota) -> bool {
    !matches!(q.window, QuotaWindow::ResetCredits)
}

/// 并行池：语义由 provider 声明（见 [`QuotaPoolSemantics`]），且快照里确实带有
/// `1st` / Credits / `API` 任一产品池窗口。窗口检查保留——纯叠加账号即使被误标并行
/// 也不会进并行分支。
fn parallel_pools(semantics: QuotaPoolSemantics, quotas: &[&Quota]) -> bool {
    semantics == QuotaPoolSemantics::Parallel
        && quotas.iter().any(|q| {
            matches!(
                q.window,
                QuotaWindow::FirstPartyModels | QuotaWindow::Credits | QuotaWindow::Api
            )
        })
}

fn auto_swap_quotas(quotas: &[Quota]) -> impl Iterator<Item = &Quota> {
    let has_gating = quotas.iter().any(quota_gates_auto_swap);
    quotas
        .iter()
        .filter(move |q| !has_gating || quota_gates_auto_swap(q))
}

fn compare_candidates(a: &AccountWithQuotas, b: &AccountWithQuotas) -> std::cmp::Ordering {
    // 用户 priority 必须排第一：排在重置时间之后时，切走会先落到非偏好账号，
    // 下一轮再被 `preferred_return_target` 拉回，平白多一次切换。
    // 同优先级内：哪个窗口最快重置就优先选谁——尽快用完即将清零的额度。
    // 没有 reset_at 信息（多见于测试 mock）时退化为「最忙窗口」used 升序（剩余多的优先）。
    let a_reset = earliest_reset(&a.quotas);
    let b_reset = earliest_reset(&b.quotas);
    a.account
        .priority
        .cmp(&b.account.priority)
        .then_with(|| compare_optional_reset(a_reset, b_reset))
        .then_with(|| busiest_used(&a.quotas).cmp(&busiest_used(&b.quotas)))
        .then(a.account.id.0.cmp(&b.account.id.0))
}

fn earliest_reset(quotas: &[Quota]) -> Option<DateTime<Utc>> {
    // 可用候选排序只看 gating 窗口：`ResetCredits` 是只读展示的过期时间，不是恢复时间。
    auto_swap_quotas(quotas).filter_map(|q| q.reset_at).min()
}

/// `None`（无重置时间信息）视为「最晚」，排在已知重置时间的候选之后。
fn compare_optional_reset(
    a: Option<DateTime<Utc>>,
    b: Option<DateTime<Utc>>,
) -> std::cmp::Ordering {
    match (a, b) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

/// `SUBSWAP_HOME` 进程锁：auto_policy 与 manual_hold 的触碰环境的测试共用。
/// 放在非 test 模块（`#[cfg(test)]` 下才编译），避免跨 `#[cfg(test)]` mod 不可见。
#[cfg(test)]
use std::sync::{Mutex, OnceLock};
#[cfg(test)]
pub(crate) fn hold_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// 最忙窗口的使用率分数（万分比）。Credits 存分、百分比窗口存 0~100，
/// 直接比 `used` 会让金额窗口永远显得更忙；只看 gating 窗口。
fn busiest_used(quotas: &[Quota]) -> u64 {
    auto_swap_quotas(quotas)
        .filter_map(|q| q.usage_ratio().map(|r| (r * 10_000.0).round() as u64))
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::hold_test_lock;
    use super::*;
    use crate::model::{AccountId, Quota, QuotaStatus, QuotaWindow};

    fn mk_account(id: &str, active: bool) -> Account {
        Account {
            provider: "claude".into(),
            id: AccountId(id.into()),
            label: id.into(),
            active,
            created_at: chrono::Utc::now(),
            last_used_at: None,
            priority: 100,
            reserve_pct: 0,
            weekly_reset: None,
            extra: serde_json::Map::new(),
        }
    }

    fn mk_quota(used: u64, status: QuotaStatus) -> Quota {
        mk_quota_with_reset(used, status, None)
    }

    fn mk_quota_with_reset(
        used: u64,
        status: QuotaStatus,
        reset_at: Option<chrono::DateTime<Utc>>,
    ) -> Quota {
        mk_quota_with_window(used, status, QuotaWindow::FiveHour, reset_at)
    }

    fn mk_quota_with_window(
        used: u64,
        status: QuotaStatus,
        window: QuotaWindow,
        reset_at: Option<chrono::DateTime<Utc>>,
    ) -> Quota {
        Quota {
            provider: "claude".into(),
            account_id: AccountId("x".into()),
            window,
            used,
            limit: 100,
            reset_at,
            status,
            note: None,
        }
    }

    fn mk_awq(id: &str, active: bool, used: u64, status: QuotaStatus) -> AccountWithQuotas {
        AccountWithQuotas {
            account: mk_account(id, active),
            quotas: vec![mk_quota(used, status)],
            fetch_state: QuotaFetchState::Ready,
        }
    }

    #[test]
    fn noop_when_active_below_threshold() {
        let snap = ProviderSnapshot {
            provider: "claude".into(),
            pool_semantics: QuotaPoolSemantics::Stacked,
            accounts: vec![
                mk_awq("a", true, 50, QuotaStatus::Ok),
                mk_awq("b", false, 0, QuotaStatus::Ok),
            ],
        };
        let d = decide(&snap, &test_config(60_000));
        assert!(matches!(d, PolicyDecision::NoOp { .. }));
    }

    #[test]
    fn swap_when_active_above_threshold() {
        let snap = ProviderSnapshot {
            provider: "claude".into(),
            pool_semantics: QuotaPoolSemantics::Stacked,
            accounts: vec![
                mk_awq("a", true, 99, QuotaStatus::Warn),
                mk_awq("b", false, 10, QuotaStatus::Ok),
                mk_awq("c", false, 30, QuotaStatus::Ok),
            ],
        };
        let d = decide(&snap, &test_config(60_000));
        match d {
            PolicyDecision::Swap { from, to, .. } => {
                assert_eq!(from.unwrap().0, "a");
                // b 剩余最多
                assert_eq!(to.0, "b");
            }
            other => panic!("expected Swap, got {other:?}"),
        }
    }

    /// 回归锚点:确认 decide() 对 provider 字符串本身没有白名单/特判——
    /// Kimi 接入共享引擎后应与 claude/codex 走完全相同的自动切换判定路径。
    #[test]
    fn kimi_provider_swaps_identically_to_other_providers() {
        let snap = ProviderSnapshot {
            provider: "kimi".into(),
            pool_semantics: QuotaPoolSemantics::Stacked,
            accounts: vec![
                mk_awq("a", true, 99, QuotaStatus::Warn),
                mk_awq("b", false, 10, QuotaStatus::Ok),
                mk_awq("c", false, 30, QuotaStatus::Ok),
            ],
        };
        let d = decide(&snap, &test_config(60_000));
        match d {
            PolicyDecision::Swap { from, to, .. } => {
                assert_eq!(from.unwrap().0, "a");
                assert_eq!(to.0, "b");
            }
            other => panic!("expected Swap for kimi provider, got {other:?}"),
        }
    }

    #[test]
    fn seven_day_threshold_does_not_trigger_auto_swap() {
        let mut active = mk_awq("a", true, 99, QuotaStatus::Warn);
        active.quotas = vec![mk_quota_with_window(
            99,
            QuotaStatus::Warn,
            QuotaWindow::SevenDay,
            None,
        )];
        let snap = ProviderSnapshot {
            provider: "claude".into(),
            pool_semantics: QuotaPoolSemantics::Stacked,
            accounts: vec![active, mk_awq("b", false, 10, QuotaStatus::Ok)],
        };
        let d = decide(&snap, &test_config(60_000));
        assert!(matches!(d, PolicyDecision::NoOp { .. }), "got {d:?}");
    }

    /// Codex 重置道具只读展示：耗尽号即使有 reset 仍视为耗尽（可被切走），
    /// 仅有 reset、无可用额度窗口的账号不能成为候选。
    #[test]
    fn reset_credits_window_never_gates_auto_swap() {
        fn reset_quota(available: u64) -> Quota {
            Quota {
                provider: "codex".into(),
                account_id: AccountId("x".into()),
                window: QuotaWindow::ResetCredits,
                used: available,
                limit: 0,
                reset_at: None,
                status: QuotaStatus::Ok,
                note: None,
            }
        }
        // 耗尽 + 有 reset → 仍触发切走。
        let mut exhausted = mk_awq("a", true, 100, QuotaStatus::Exhausted);
        exhausted.quotas.push(reset_quota(2));
        let snap = ProviderSnapshot {
            provider: "codex".into(),
            pool_semantics: QuotaPoolSemantics::Stacked,
            accounts: vec![exhausted, mk_awq("b", false, 10, QuotaStatus::Ok)],
        };
        match decide(&snap, &test_config(60_000)) {
            PolicyDecision::Swap { to, .. } => assert_eq!(to.0, "b"),
            other => panic!("exhausted active with resets must still swap, got {other:?}"),
        }
        // 仅 reset、无额度窗口 → 不能当候选。
        let reset_only = AccountWithQuotas {
            account: mk_account("c", false),
            quotas: vec![reset_quota(3)],
            fetch_state: QuotaFetchState::Ready,
        };
        let snap = ProviderSnapshot {
            provider: "codex".into(),
            pool_semantics: QuotaPoolSemantics::Stacked,
            accounts: vec![mk_awq("a", true, 100, QuotaStatus::Exhausted), reset_only],
        };
        assert!(
            matches!(
                decide(&snap, &test_config(60_000)),
                PolicyDecision::Degraded { .. }
            ),
            "reset-only account must not be a swap candidate"
        );
    }

    #[test]
    fn seven_day_threshold_does_not_block_candidate() {
        let mut candidate = mk_awq("b", false, 10, QuotaStatus::Ok);
        candidate.quotas.push(mk_quota_with_window(
            99,
            QuotaStatus::Warn,
            QuotaWindow::SevenDay,
            None,
        ));
        let snap = ProviderSnapshot {
            provider: "claude".into(),
            pool_semantics: QuotaPoolSemantics::Stacked,
            accounts: vec![mk_awq("a", true, 99, QuotaStatus::Warn), candidate],
        };
        let d = decide(&snap, &test_config(60_000));
        assert!(
            matches!(d, PolicyDecision::Swap { ref to, .. } if to.0 == "b"),
            "got {d:?}"
        );
    }

    #[test]
    fn seven_day_exhausted_still_triggers_auto_swap() {
        let mut active = mk_awq("a", true, 100, QuotaStatus::Exhausted);
        active.quotas = vec![mk_quota_with_window(
            100,
            QuotaStatus::Exhausted,
            QuotaWindow::SevenDay,
            None,
        )];
        let snap = ProviderSnapshot {
            provider: "claude".into(),
            pool_semantics: QuotaPoolSemantics::Stacked,
            accounts: vec![active, mk_awq("b", false, 10, QuotaStatus::Ok)],
        };
        let d = decide(&snap, &test_config(60_000));
        assert!(
            matches!(d, PolicyDecision::Swap { ref to, .. } if to.0 == "b"),
            "got {d:?}"
        );
    }

    #[test]
    fn degraded_when_all_candidates_exhausted() {
        let snap = ProviderSnapshot {
            provider: "claude".into(),
            pool_semantics: QuotaPoolSemantics::Stacked,
            accounts: vec![
                mk_awq("a", true, 100, QuotaStatus::Exhausted),
                mk_awq("b", false, 100, QuotaStatus::Exhausted),
            ],
        };
        let d = decide(&snap, &test_config(60_000));
        assert!(matches!(d, PolicyDecision::Degraded { .. }));
    }

    /// 嵌套窗口（2026-10-08 用户现场）：两号 `5h` 都有余量、`7d` 都耗尽。
    /// 大窗口包含小窗口，`7d` 耗尽时 `5h` 余量不算数；有效恢复取阻塞窗（`7d`）最晚值。
    /// active `5h` 47m 后重置也不能先恢复，候选 `7d` 48h < active `7d` 3d → 切到候选。
    #[test]
    fn nested_7d_exhausted_falls_back_to_sooner_7d_recovery() {
        let now = chrono::Utc::now();
        let mut active = mk_awq("a", true, 9, QuotaStatus::Ok);
        active.quotas = vec![
            mk_quota_with_window(
                9,
                QuotaStatus::Ok,
                QuotaWindow::FiveHour,
                Some(now + chrono::Duration::minutes(47)),
            ),
            mk_quota_with_window(
                100,
                QuotaStatus::Exhausted,
                QuotaWindow::SevenDay,
                Some(now + chrono::Duration::days(3)),
            ),
        ];
        let mut candidate = mk_awq("b", false, 0, QuotaStatus::Ok);
        candidate.quotas = vec![
            mk_quota_with_window(0, QuotaStatus::Ok, QuotaWindow::FiveHour, None),
            mk_quota_with_window(
                100,
                QuotaStatus::Exhausted,
                QuotaWindow::SevenDay,
                Some(now + chrono::Duration::hours(48)),
            ),
        ];
        let snap = ProviderSnapshot {
            provider: "claude".into(),
            pool_semantics: QuotaPoolSemantics::Stacked,
            accounts: vec![active, candidate],
        };
        match decide(&snap, &test_config(60_000)) {
            PolicyDecision::Swap { from, to, .. } => {
                assert_eq!(from.unwrap().0, "a");
                assert_eq!(to.0, "b");
            }
            other => panic!("nested 7d exhausted must swap to sooner 7d recovery, got {other:?}"),
        }
    }

    /// 嵌套窗口：阻塞窗缺 `reset_at` 则恢复时间未知，不得入回退池。
    /// 候选 `7d` 耗尽但无重置时间 → 池空 → `Degraded`，不能按 `5h` 已知重置硬切。
    #[test]
    fn nested_blocking_window_missing_reset_is_excluded() {
        let now = chrono::Utc::now();
        let mut active = mk_awq("a", true, 9, QuotaStatus::Ok);
        active.quotas = vec![
            mk_quota_with_window(
                9,
                QuotaStatus::Ok,
                QuotaWindow::FiveHour,
                Some(now + chrono::Duration::minutes(47)),
            ),
            mk_quota_with_window(
                100,
                QuotaStatus::Exhausted,
                QuotaWindow::SevenDay,
                Some(now + chrono::Duration::days(3)),
            ),
        ];
        let mut candidate = mk_awq("b", false, 0, QuotaStatus::Ok);
        candidate.quotas = vec![
            mk_quota_with_window(0, QuotaStatus::Ok, QuotaWindow::FiveHour, None),
            mk_quota_with_window(100, QuotaStatus::Exhausted, QuotaWindow::SevenDay, None),
        ];
        let snap = ProviderSnapshot {
            provider: "claude".into(),
            pool_semantics: QuotaPoolSemantics::Stacked,
            accounts: vec![active, candidate],
        };
        let d = decide(&snap, &test_config(60_000));
        assert!(matches!(d, PolicyDecision::Degraded { .. }), "got {d:?}");
    }

    /// 嵌套窗口：双窗都耗尽时恢复取阻塞中最晚值。
    /// active `5h 4h + 7d 3d` → 3d；候选 `5h 2m + 7d 4d` → 4d；当前更快 → 留守。
    /// 按旧 `min(全部窗口)` 会误算成 `4h vs 2m` 而硬切。
    #[test]
    fn nested_both_exhausted_uses_latest_blocking_reset() {
        let now = chrono::Utc::now();
        let mut active = mk_awq("a", true, 100, QuotaStatus::Exhausted);
        active.quotas = vec![
            mk_quota_with_window(
                100,
                QuotaStatus::Exhausted,
                QuotaWindow::FiveHour,
                Some(now + chrono::Duration::hours(4)),
            ),
            mk_quota_with_window(
                100,
                QuotaStatus::Exhausted,
                QuotaWindow::SevenDay,
                Some(now + chrono::Duration::days(3)),
            ),
        ];
        let mut candidate = mk_awq("b", false, 100, QuotaStatus::Exhausted);
        candidate.quotas = vec![
            mk_quota_with_window(
                100,
                QuotaStatus::Exhausted,
                QuotaWindow::FiveHour,
                Some(now + chrono::Duration::minutes(2)),
            ),
            mk_quota_with_window(
                100,
                QuotaStatus::Exhausted,
                QuotaWindow::SevenDay,
                Some(now + chrono::Duration::days(4)),
            ),
        ];
        let snap = ProviderSnapshot {
            provider: "claude".into(),
            pool_semantics: QuotaPoolSemantics::Stacked,
            accounts: vec![active, candidate],
        };
        let d = decide(&snap, &test_config(60_000));
        assert!(
            matches!(d, PolicyDecision::NoOp { ref reason, .. } if reason.contains("recovers soonest")),
            "got {d:?}"
        );
    }

    /// 全员耗尽回退（2026-09-30 用户决策）：当前号 5h 耗尽、候选 5h 也耗尽但恢复更快 → 切过去。
    /// 复现真实快照：active 5h 0%（4h 后恢复），candidate 5h 0%（2m 后恢复）+ reset 道具列。
    #[test]
    fn all_exhausted_falls_back_to_soonest_recovery() {
        let now = chrono::Utc::now();
        let hours4 = now + chrono::Duration::hours(4);
        let mins2 = now + chrono::Duration::minutes(2);
        let days7 = now + chrono::Duration::days(7);
        let mut active = mk_awq("a", true, 100, QuotaStatus::Exhausted);
        active.quotas = vec![
            mk_quota_with_window(
                100,
                QuotaStatus::Exhausted,
                QuotaWindow::FiveHour,
                Some(hours4),
            ),
            mk_quota_with_window(16, QuotaStatus::Ok, QuotaWindow::SevenDay, Some(days7)),
        ];
        let mut candidate = mk_awq("b", false, 100, QuotaStatus::Exhausted);
        candidate.quotas = vec![
            mk_quota_with_window(
                100,
                QuotaStatus::Exhausted,
                QuotaWindow::FiveHour,
                Some(mins2),
            ),
            mk_quota_with_window(31, QuotaStatus::Ok, QuotaWindow::SevenDay, Some(days7)),
            Quota {
                provider: "codex".into(),
                account_id: AccountId("x".into()),
                window: QuotaWindow::ResetCredits,
                used: 1,
                limit: 0,
                reset_at: Some(now + chrono::Duration::days(30)),
                status: QuotaStatus::Ok,
                note: None,
            },
        ];
        let snap = ProviderSnapshot {
            provider: "codex".into(),
            pool_semantics: QuotaPoolSemantics::Stacked,
            accounts: vec![active, candidate],
        };
        match decide(&snap, &test_config(60_000)) {
            PolicyDecision::Swap { from, to, .. } => {
                assert_eq!(from.unwrap().0, "a");
                assert_eq!(to.0, "b");
            }
            other => panic!("all exhausted must fall back to soonest recovery, got {other:?}"),
        }
    }

    /// 池非空但当前号恢复最快（候选更晚）→ 留守 NoOp，不 Degraded。
    #[test]
    fn fallback_stays_when_current_recovers_soonest() {
        let now = chrono::Utc::now();
        let mut active = mk_awq("a", true, 100, QuotaStatus::Exhausted);
        active.quotas = vec![mk_quota_with_window(
            100,
            QuotaStatus::Exhausted,
            QuotaWindow::FiveHour,
            Some(now + chrono::Duration::minutes(2)),
        )];
        let mut candidate = mk_awq("b", false, 100, QuotaStatus::Exhausted);
        candidate.quotas = vec![mk_quota_with_window(
            100,
            QuotaStatus::Exhausted,
            QuotaWindow::FiveHour,
            Some(now + chrono::Duration::hours(4)),
        )];
        let snap = ProviderSnapshot {
            provider: "codex".into(),
            pool_semantics: QuotaPoolSemantics::Stacked,
            accounts: vec![active, candidate],
        };
        let d = decide(&snap, &test_config(60_000));
        assert!(
            matches!(d, PolicyDecision::NoOp { ref reason, .. } if reason.contains("recovers soonest")),
            "got {d:?}"
        );
    }

    /// 回退池排除未知 / 失败 / manual_only 账号：只剩它们 → Degraded。
    #[test]
    fn fallback_ignores_unknown_failed_and_manual_only() {
        let now = chrono::Utc::now();
        let reset = Some(now + chrono::Duration::minutes(2));
        let mut active = mk_awq("a", true, 100, QuotaStatus::Exhausted);
        active.quotas = vec![mk_quota_with_window(
            100,
            QuotaStatus::Exhausted,
            QuotaWindow::FiveHour,
            Some(now + chrono::Duration::hours(4)),
        )];
        // b：窗口未知（limit 0 / Unknown），不可比。
        let mut unknown = mk_awq("b", false, 100, QuotaStatus::Exhausted);
        unknown.quotas = vec![mk_quota_with_window(
            0,
            QuotaStatus::Unknown,
            QuotaWindow::FiveHour,
            reset,
        )];
        unknown.quotas[0].limit = 0;
        // c：查询失败。
        let mut failed = mk_awq("c", false, 100, QuotaStatus::Exhausted);
        failed.quotas = vec![mk_quota_with_window(
            100,
            QuotaStatus::Exhausted,
            QuotaWindow::FiveHour,
            reset,
        )];
        failed.fetch_state = QuotaFetchState::Failed("timeout".into());
        // d：manual_only，即使恢复更快也不入池。
        let mut held = mk_awq("d", false, 100, QuotaStatus::Exhausted);
        held.quotas = vec![mk_quota_with_window(
            100,
            QuotaStatus::Exhausted,
            QuotaWindow::FiveHour,
            reset,
        )];
        held.account.extra.insert("manual_only".into(), true.into());
        let snap = ProviderSnapshot {
            provider: "codex".into(),
            pool_semantics: QuotaPoolSemantics::Stacked,
            accounts: vec![active, unknown, failed, held],
        };
        let d = decide(&snap, &test_config(60_000));
        assert!(matches!(d, PolicyDecision::Degraded { .. }), "got {d:?}");
    }

    /// 仅 Warn（未耗尽）的当前号 + 其他全耗尽 → 不回退（当前仍可服务），Degraded。
    #[test]
    fn fallback_never_leaves_warn_only_active_for_depleted() {
        let now = chrono::Utc::now();
        let mut active = mk_awq("a", true, 99, QuotaStatus::Warn);
        active.quotas = vec![mk_quota_with_window(
            99,
            QuotaStatus::Warn,
            QuotaWindow::FiveHour,
            Some(now + chrono::Duration::hours(4)),
        )];
        let mut candidate = mk_awq("b", false, 100, QuotaStatus::Exhausted);
        candidate.quotas = vec![mk_quota_with_window(
            100,
            QuotaStatus::Exhausted,
            QuotaWindow::FiveHour,
            Some(now + chrono::Duration::minutes(2)),
        )];
        let snap = ProviderSnapshot {
            provider: "codex".into(),
            pool_semantics: QuotaPoolSemantics::Stacked,
            accounts: vec![active, candidate],
        };
        let d = decide(&snap, &test_config(60_000));
        assert!(matches!(d, PolicyDecision::Degraded { .. }), "got {d:?}");
    }

    /// 无当前号 + 全耗尽 → 激活恢复最快的。
    #[test]
    fn fallback_without_active_activates_soonest() {
        let now = chrono::Utc::now();
        let mut slow = mk_awq("slow", false, 100, QuotaStatus::Exhausted);
        slow.quotas = vec![mk_quota_with_window(
            100,
            QuotaStatus::Exhausted,
            QuotaWindow::FiveHour,
            Some(now + chrono::Duration::hours(4)),
        )];
        let mut fast = mk_awq("fast", false, 100, QuotaStatus::Exhausted);
        fast.quotas = vec![mk_quota_with_window(
            100,
            QuotaStatus::Exhausted,
            QuotaWindow::FiveHour,
            Some(now + chrono::Duration::minutes(2)),
        )];
        let snap = ProviderSnapshot {
            provider: "codex".into(),
            pool_semantics: QuotaPoolSemantics::Stacked,
            accounts: vec![slow, fast],
        };
        match decide(&snap, &test_config(60_000)) {
            PolicyDecision::Swap { from, to, .. } => {
                assert!(from.is_none());
                assert_eq!(to.0, "fast");
            }
            other => panic!("expected fallback activation, got {other:?}"),
        }
    }

    #[test]
    fn active_quota_fetch_failure_keeps_current_account() {
        let mut a = mk_awq("a", true, 0, QuotaStatus::Unknown);
        a.fetch_state = QuotaFetchState::Failed("timeout".into());
        let snap = ProviderSnapshot {
            provider: "claude".into(),
            pool_semantics: QuotaPoolSemantics::Stacked,
            accounts: vec![a, mk_awq("b", false, 0, QuotaStatus::Ok)],
        };
        let d = decide(&snap, &test_config(60_000));
        assert!(matches!(d, PolicyDecision::Degraded { .. }));
    }

    #[test]
    fn active_quota_loading_keeps_current_account() {
        let mut a = mk_awq("a", true, 0, QuotaStatus::Unknown);
        a.quotas.clear();
        a.fetch_state = QuotaFetchState::Loading;
        let snap = ProviderSnapshot {
            provider: "claude".into(),
            pool_semantics: QuotaPoolSemantics::Stacked,
            accounts: vec![a, mk_awq("b", false, 0, QuotaStatus::Ok)],
        };
        let d = decide(&snap, &test_config(60_000));
        assert!(matches!(d, PolicyDecision::NoOp { .. }));
    }

    /// 手动保持：保持期内连「已明确耗尽」的确定性切换一起挡（settle grace 只挡不确定状态）。
    #[test]
    fn manual_hold_blocks_even_exhausted_active() {
        let _guard = hold_test_lock().lock().unwrap();
        let prev = std::env::var_os("SUBSWAP_HOME");
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("SUBSWAP_HOME", tmp.path().join("subswap"));
        crate::manual_hold::record_manual_swap_with_hold("claude", 600_000).unwrap();
        let snap = ProviderSnapshot {
            provider: "claude".into(),
            pool_semantics: QuotaPoolSemantics::Stacked,
            accounts: vec![
                mk_awq("a", true, 100, QuotaStatus::Exhausted),
                mk_awq("b", false, 10, QuotaStatus::Ok),
            ],
        };
        let mut cfg = test_config(60_000);
        cfg.manual_hold_ms = 600_000;
        let d = decide(&snap, &cfg);
        assert!(
            matches!(d, PolicyDecision::NoOp { ref reason } if reason.contains("manually selected")),
            "got {d:?}"
        );
        // 保持关闭（0）时同一快照恢复确定性切换。
        let cfg = test_config(60_000);
        let d = decide(&snap, &cfg);
        assert!(
            matches!(d, PolicyDecision::Swap { ref to, .. } if to.0 == "b"),
            "got {d:?}"
        );
        match prev {
            Some(v) => std::env::set_var("SUBSWAP_HOME", v),
            None => std::env::remove_var("SUBSWAP_HOME"),
        }
    }

    #[test]
    fn rejection_overrides_manual_hold() {
        let _guard = hold_test_lock().lock().unwrap();
        let prev = std::env::var_os("SUBSWAP_HOME");
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("SUBSWAP_HOME", tmp.path().join("subswap"));
        crate::manual_hold::record_manual_swap_with_hold("claude", 600_000).unwrap();
        let snap = ProviderSnapshot {
            provider: "claude".into(),
            pool_semantics: QuotaPoolSemantics::Stacked,
            accounts: vec![
                mk_windows(
                    "work",
                    true,
                    100,
                    &[
                        (QuotaWindow::FiveHour, 10, QuotaStatus::Ok, Some(2)),
                        (QuotaWindow::Rejected, 100, QuotaStatus::Exhausted, Some(40)),
                    ],
                ),
                mk_awq("personal", false, 10, QuotaStatus::Ok),
            ],
        };
        let mut cfg = test_config(0);
        cfg.manual_hold_ms = 600_000;
        let d = decide(&snap, &cfg);
        match prev {
            Some(v) => std::env::set_var("SUBSWAP_HOME", v),
            None => std::env::remove_var("SUBSWAP_HOME"),
        }
        assert!(
            matches!(d, PolicyDecision::Swap { ref to, .. } if to.0 == "personal"),
            "got {d:?}"
        );
    }

    /// 刚激活的账号 quota 还在 loading 时，沉淀宽限期内不应被自动切走
    /// （否则手动 swap 会被一次 `subswap` 或 daemon 立刻顶掉）。
    #[test]
    fn just_activated_loading_account_is_not_swapped_away() {
        let mut a = mk_awq("a", true, 0, QuotaStatus::Unknown);
        a.quotas.clear();
        a.fetch_state = QuotaFetchState::Loading;
        a.account.last_used_at = Some(Utc::now());
        let snap = ProviderSnapshot {
            provider: "claude".into(),
            pool_semantics: QuotaPoolSemantics::Stacked,
            accounts: vec![a, mk_awq("b", false, 0, QuotaStatus::Ok)],
        };
        let cfg = test_config(60_000);
        let d = decide(&snap, &cfg);
        assert!(matches!(d, PolicyDecision::NoOp { .. }), "got {d:?}");
    }

    /// 刚激活的账号 quota 拉取失败时，同样在宽限期内不被切走。
    #[test]
    fn just_activated_failed_account_is_not_swapped_away() {
        let mut a = mk_awq("a", true, 0, QuotaStatus::Unknown);
        a.fetch_state = QuotaFetchState::Failed("timeout".into());
        a.account.last_used_at = Some(Utc::now());
        let snap = ProviderSnapshot {
            provider: "claude".into(),
            pool_semantics: QuotaPoolSemantics::Stacked,
            accounts: vec![a, mk_awq("b", false, 0, QuotaStatus::Ok)],
        };
        let cfg = test_config(60_000);
        let d = decide(&snap, &cfg);
        assert!(matches!(d, PolicyDecision::Degraded { .. }), "got {d:?}");
    }

    /// 宽限期只保护「不确定状态」；账号已明确达到 threshold 时仍按确定性数据切走。
    #[test]
    fn just_activated_but_exhausted_account_still_swaps() {
        let mut a = mk_awq("a", true, 99, QuotaStatus::Warn);
        a.account.last_used_at = Some(Utc::now());
        let snap = ProviderSnapshot {
            provider: "claude".into(),
            pool_semantics: QuotaPoolSemantics::Stacked,
            accounts: vec![a, mk_awq("b", false, 10, QuotaStatus::Ok)],
        };
        let cfg = test_config(60_000);
        let d = decide(&snap, &cfg);
        assert!(
            matches!(d, PolicyDecision::Swap { ref to, .. } if to.0 == "b"),
            "got {d:?}"
        );
    }

    /// 不确定额度永远不能触发切换，宽限期结束也不能当作耗尽证据。
    #[test]
    fn loading_account_stays_after_grace_window_elapses() {
        let mut a = mk_awq("a", true, 0, QuotaStatus::Unknown);
        a.quotas.clear();
        a.fetch_state = QuotaFetchState::Loading;
        a.account.last_used_at = Some(Utc::now() - chrono::Duration::seconds(120));
        let snap = ProviderSnapshot {
            provider: "claude".into(),
            pool_semantics: QuotaPoolSemantics::Stacked,
            accounts: vec![a, mk_awq("b", false, 0, QuotaStatus::Ok)],
        };
        let cfg = test_config(60_000);
        let d = decide(&snap, &cfg);
        assert!(matches!(d, PolicyDecision::NoOp { .. }), "got {d:?}");
    }

    #[test]
    fn active_manual_only_account_disables_auto_swap_while_loading() {
        let mut api = mk_awq("api", true, 0, QuotaStatus::Unknown);
        api.account.extra.insert("manual_only".into(), true.into());
        api.quotas.clear();
        api.fetch_state = QuotaFetchState::Loading;
        let snap = ProviderSnapshot {
            provider: "claude".into(),
            pool_semantics: QuotaPoolSemantics::Stacked,
            accounts: vec![api, mk_awq("oauth", false, 0, QuotaStatus::Ok)],
        };
        let d = decide(&snap, &test_config(60_000));
        assert!(matches!(d, PolicyDecision::NoOp { .. }));
    }

    #[test]
    fn manual_only_account_is_never_an_auto_swap_candidate() {
        let mut api = mk_awq("api", false, 0, QuotaStatus::Ok);
        api.account.extra.insert("manual_only".into(), true.into());
        let snap = ProviderSnapshot {
            provider: "claude".into(),
            pool_semantics: QuotaPoolSemantics::Stacked,
            accounts: vec![mk_awq("oauth", true, 100, QuotaStatus::Exhausted), api],
        };
        let d = decide(&snap, &test_config(60_000));
        assert!(matches!(d, PolicyDecision::Degraded { .. }));
    }

    #[test]
    fn degraded_when_active_quota_fetch_fails_without_known_candidate() {
        let mut a = mk_awq("a", true, 0, QuotaStatus::Unknown);
        a.fetch_state = QuotaFetchState::Failed("timeout".into());
        let mut b = mk_awq("b", false, 0, QuotaStatus::Unknown);
        b.fetch_state = QuotaFetchState::Failed("429".into());
        let snap = ProviderSnapshot {
            provider: "claude".into(),
            pool_semantics: QuotaPoolSemantics::Stacked,
            accounts: vec![a, b],
        };
        let d = decide(&snap, &test_config(60_000));
        assert!(matches!(d, PolicyDecision::Degraded { .. }));
    }

    #[test]
    fn activates_when_no_active_account() {
        let snap = ProviderSnapshot {
            provider: "claude".into(),
            pool_semantics: QuotaPoolSemantics::Stacked,
            accounts: vec![
                mk_awq("a", false, 20, QuotaStatus::Ok),
                mk_awq("b", false, 5, QuotaStatus::Ok),
            ],
        };
        let d = decide(&snap, &test_config(60_000));
        match d {
            PolicyDecision::Swap { from, to, .. } => {
                assert!(from.is_none());
                assert_eq!(to.0, "b"); // 用得少的优先
            }
            other => panic!("expected Swap, got {other:?}"),
        }
    }

    #[test]
    fn candidate_also_above_threshold_yields_degraded_not_churn() {
        // 用户实际场景：两个号都接近耗尽时不应该硬切。
        let snap = ProviderSnapshot {
            provider: "codex".into(),
            pool_semantics: QuotaPoolSemantics::Stacked,
            accounts: vec![
                mk_awq("a", true, 100, QuotaStatus::Exhausted),
                mk_awq("b", false, 99, QuotaStatus::Warn),
            ],
        };
        let d = decide(&snap, &test_config(60_000));
        assert!(matches!(d, PolicyDecision::Degraded { .. }), "got {d:?}");
    }

    #[test]
    fn exhausted_active_does_not_swap_to_failed_quota_candidate() {
        let mut candidate = mk_awq("candidate", false, 0, QuotaStatus::Unknown);
        candidate.quotas.clear();
        candidate.fetch_state = QuotaFetchState::Failed("429 rate limited".into());

        let d = decide(
            &ProviderSnapshot {
                provider: "claude".into(),
                pool_semantics: QuotaPoolSemantics::Stacked,
                accounts: vec![
                    mk_awq("active", true, 100, QuotaStatus::Exhausted),
                    candidate,
                ],
            },
            &test_config(60_000),
        );
        assert!(matches!(d, PolicyDecision::Degraded { .. }));
    }

    #[test]
    fn exhausted_active_does_not_swap_to_stale_relogin_candidate() {
        let mut candidate = mk_awq("candidate", false, 0, QuotaStatus::Ok);
        candidate.fetch_state = QuotaFetchState::Stale {
            cached_at: Utc::now() - chrono::Duration::hours(39),
            error: "re-login required; access token missing".into(),
        };

        let d = decide(
            &ProviderSnapshot {
                provider: "claude".into(),
                pool_semantics: QuotaPoolSemantics::Stacked,
                accounts: vec![
                    mk_awq("active", true, 100, QuotaStatus::Exhausted),
                    candidate,
                ],
            },
            &test_config(60_000),
        );
        assert!(matches!(d, PolicyDecision::Degraded { .. }));
    }

    #[test]
    fn exhausted_active_does_not_use_failed_relogin_fallback() {
        let mut candidate = mk_awq("candidate", false, 0, QuotaStatus::Ok);
        candidate.fetch_state =
            QuotaFetchState::Failed("re-login required; access token missing".into());

        let d = decide(
            &ProviderSnapshot {
                provider: "claude".into(),
                pool_semantics: QuotaPoolSemantics::Stacked,
                accounts: vec![
                    mk_awq("active", true, 100, QuotaStatus::Exhausted),
                    candidate,
                ],
            },
            &test_config(60_000),
        );
        assert!(matches!(d, PolicyDecision::Degraded { .. }));
    }

    /// 候选都有额度（未达阈值）时，应优先选窗口最快重置的，而不是单纯剩余最多的。
    #[test]
    fn swap_prefers_soonest_reset_candidate_over_more_headroom() {
        let now = Utc::now();
        let mut active = mk_awq("a", true, 99, QuotaStatus::Warn);
        active.quotas[0].reset_at = Some(now + chrono::Duration::hours(4));

        let mut more_headroom = mk_awq("b", false, 22, QuotaStatus::Ok);
        more_headroom.quotas[0].reset_at = Some(now + chrono::Duration::hours(4));

        let mut soonest_reset = mk_awq("c", false, 43, QuotaStatus::Ok);
        soonest_reset.quotas[0].reset_at = Some(now + chrono::Duration::hours(2));

        let snap = ProviderSnapshot {
            provider: "claude".into(),
            pool_semantics: QuotaPoolSemantics::Stacked,
            accounts: vec![active, more_headroom, soonest_reset],
        };
        let d = decide(&snap, &test_config(60_000));
        match d {
            // c 剩余更少，但重置更快，应该被优先选中
            PolicyDecision::Swap { to, .. } => assert_eq!(to.0, "c"),
            other => panic!("expected Swap, got {other:?}"),
        }
    }

    fn mk_windows(
        id: &str,
        active: bool,
        priority: i32,
        windows: &[(QuotaWindow, u64, QuotaStatus, Option<i64>)],
    ) -> AccountWithQuotas {
        let now = Utc::now();
        let mut awq = mk_awq(id, active, 0, QuotaStatus::Ok);
        awq.account.priority = priority;
        awq.quotas = windows
            .iter()
            .map(|(window, used, status, hours)| {
                mk_quota_with_window(
                    *used,
                    *status,
                    *window,
                    hours.map(|h| now + chrono::Duration::hours(h)),
                )
            })
            .collect();
        awq
    }

    fn stacked(accounts: Vec<AccountWithQuotas>) -> ProviderSnapshot {
        ProviderSnapshot {
            provider: "claude".into(),
            pool_semantics: QuotaPoolSemantics::Stacked,
            accounts,
        }
    }

    use QuotaWindow::{FiveHour as H5, ModelWeek as MW, SevenDay as D7};

    #[test]
    fn healthy_active_returns_to_preferred_with_headroom() {
        let snap = stacked(vec![
            mk_windows("work", true, 100, &[(H5, 50, QuotaStatus::Ok, Some(2))]),
            mk_windows(
                "personal",
                false,
                10,
                &[
                    (H5, 0, QuotaStatus::Ok, None),
                    (D7, 20, QuotaStatus::Ok, Some(100)),
                ],
            ),
        ]);
        match decide(&snap, &test_config(0)) {
            PolicyDecision::Swap { from, to, reason } => {
                assert_eq!(from.unwrap().0, "work");
                assert_eq!(to.0, "personal");
                assert!(reason.contains("return to preferred"), "{reason}");
            }
            other => panic!("expected Swap, got {other:?}"),
        }
    }

    #[test]
    fn no_return_when_priorities_equal() {
        let snap = stacked(vec![
            mk_windows("work", true, 100, &[(H5, 50, QuotaStatus::Ok, Some(2))]),
            mk_windows("personal", false, 100, &[(H5, 0, QuotaStatus::Ok, None)]),
        ]);
        assert!(matches!(
            decide(&snap, &test_config(0)),
            PolicyDecision::NoOp { .. }
        ));
    }

    /// 偏好账号 5h 越过回切线：不切回，否则刚切回就会触顶再切走。
    #[test]
    fn no_return_when_preferred_five_hour_lacks_headroom() {
        let snap = stacked(vec![
            mk_windows("work", true, 100, &[(H5, 50, QuotaStatus::Ok, Some(2))]),
            mk_windows(
                "personal",
                false,
                10,
                &[(H5, 92, QuotaStatus::Warn, Some(3))],
            ),
        ]);
        assert!(matches!(
            decide(&snap, &test_config(0)),
            PolicyDecision::NoOp { .. }
        ));
    }

    /// 长窗口只增不减，没有回切振荡：未耗尽就回切，把偏好账号的周额度用满。
    #[test]
    fn long_windows_below_blocking_line_still_allow_return() {
        for window in [D7, MW] {
            let snap = stacked(vec![
                mk_windows("work", true, 100, &[(H5, 50, QuotaStatus::Ok, Some(2))]),
                mk_windows(
                    "personal",
                    false,
                    10,
                    &[
                        (H5, 0, QuotaStatus::Ok, Some(3)),
                        (window, 95, QuotaStatus::Warn, Some(40)),
                    ],
                ),
            ]);
            match decide(&snap, &test_config(0)) {
                PolicyDecision::Swap { to, .. } => assert_eq!(to.0, "personal"),
                other => panic!("{window:?}: expected Swap, got {other:?}"),
            }
        }
    }

    fn with_reserve(mut a: AccountWithQuotas, reserve_pct: u8) -> AccountWithQuotas {
        a.account.reserve_pct = reserve_pct;
        a
    }

    #[test]
    fn reserve_swaps_away_before_long_window_exhausts() {
        let snap = stacked(vec![
            with_reserve(
                mk_windows(
                    "personal",
                    true,
                    10,
                    &[
                        (H5, 20, QuotaStatus::Ok, Some(3)),
                        (D7, 85, QuotaStatus::Ok, Some(40)),
                    ],
                ),
                15,
            ),
            mk_windows("work", false, 100, &[(H5, 0, QuotaStatus::Ok, None)]),
        ]);
        match decide(&snap, &test_config(0)) {
            PolicyDecision::Swap { to, .. } => assert_eq!(to.0, "work"),
            other => panic!("expected Swap, got {other:?}"),
        }
    }

    #[test]
    fn reserve_swaps_away_on_five_hour_before_threshold() {
        let snap = stacked(vec![
            with_reserve(
                mk_windows(
                    "personal",
                    true,
                    10,
                    &[(H5, 90, QuotaStatus::Warn, Some(3))],
                ),
                10,
            ),
            mk_windows("work", false, 100, &[(H5, 0, QuotaStatus::Ok, None)]),
        ]);
        match decide(&snap, &test_config(0)) {
            PolicyDecision::Swap { to, .. } => assert_eq!(to.0, "work"),
            other => panic!("expected Swap, got {other:?}"),
        }
    }

    #[test]
    fn account_in_reserve_is_never_a_target() {
        let snap = stacked(vec![
            mk_windows("work", true, 100, &[(H5, 99, QuotaStatus::Warn, Some(2))]),
            with_reserve(
                mk_windows(
                    "personal",
                    false,
                    10,
                    &[
                        (H5, 0, QuotaStatus::Ok, None),
                        (D7, 86, QuotaStatus::Ok, Some(40)),
                    ],
                ),
                15,
            ),
        ]);
        assert!(matches!(
            decide(&snap, &test_config(0)),
            PolicyDecision::Degraded { .. }
        ));
    }

    #[test]
    fn account_below_reserve_line_is_still_a_target() {
        let snap = stacked(vec![
            mk_windows("work", true, 100, &[(H5, 99, QuotaStatus::Warn, Some(2))]),
            with_reserve(
                mk_windows(
                    "personal",
                    false,
                    10,
                    &[
                        (H5, 0, QuotaStatus::Ok, None),
                        (D7, 80, QuotaStatus::Ok, Some(40)),
                    ],
                ),
                15,
            ),
        ]);
        match decide(&snap, &test_config(0)) {
            PolicyDecision::Swap { to, .. } => assert_eq!(to.0, "personal"),
            other => panic!("expected Swap, got {other:?}"),
        }
    }

    /// 保留余量下移 5h 回切线：reserve 15 + 测试滞回 8 点 → 5h 低于 77% 才回切。
    #[test]
    fn reserve_lowers_five_hour_return_line() {
        let case = |used: u64| {
            stacked(vec![
                mk_windows("work", true, 100, &[(H5, 50, QuotaStatus::Ok, Some(2))]),
                with_reserve(
                    mk_windows(
                        "personal",
                        false,
                        10,
                        &[(H5, used, QuotaStatus::Ok, Some(3))],
                    ),
                    15,
                ),
            ])
        };
        assert!(matches!(
            decide(&case(80), &test_config(0)),
            PolicyDecision::NoOp { .. }
        ));
        assert!(matches!(
            decide(&case(70), &test_config(0)),
            PolicyDecision::Swap { .. }
        ));
    }

    /// 保留区按耗尽处理：全员不可用时切到恢复更早的耗尽号，而不是留在保留区里继续花。
    #[test]
    fn reserve_is_not_spent_when_everything_else_is_exhausted() {
        let snap = stacked(vec![
            with_reserve(
                mk_windows(
                    "personal",
                    true,
                    10,
                    &[
                        (H5, 10, QuotaStatus::Ok, Some(3)),
                        (D7, 88, QuotaStatus::Ok, Some(40)),
                    ],
                ),
                15,
            ),
            mk_windows(
                "work",
                false,
                100,
                &[(H5, 100, QuotaStatus::Exhausted, Some(2))],
            ),
        ]);
        match decide(&snap, &test_config(0)) {
            PolicyDecision::Swap { to, reason, .. } => {
                assert_eq!(to.0, "work");
                assert!(reason.contains("recovers soonest"), "{reason}");
            }
            other => panic!("expected Swap, got {other:?}"),
        }
    }

    #[test]
    fn reserve_adjustment_skips_unknown_and_zero_reserve() {
        let mut unknown = mk_quota_with_window(90, QuotaStatus::Unknown, D7, None);
        unknown.limit = 100;
        let known = mk_quota_with_window(90, QuotaStatus::Warn, D7, None);
        let mut account = mk_account("a", true);
        let untouched = reserve_adjusted_quotas(&account, std::slice::from_ref(&known));
        assert_eq!(untouched[0].status, QuotaStatus::Warn);
        account.reserve_pct = 15;
        let adjusted = reserve_adjusted_quotas(&account, &[known, unknown]);
        assert_eq!(adjusted[0].status, QuotaStatus::Exhausted);
        assert_eq!(adjusted[1].status, QuotaStatus::Unknown);
        assert_eq!(adjusted[0].used, 90);
    }

    #[test]
    fn no_return_to_exhausted_or_unconfirmed_preferred() {
        let exhausted = mk_windows(
            "personal",
            false,
            10,
            &[
                (H5, 0, QuotaStatus::Ok, None),
                (D7, 100, QuotaStatus::Exhausted, Some(40)),
            ],
        );
        let mut failed = mk_windows("personal", false, 10, &[(H5, 0, QuotaStatus::Ok, None)]);
        failed.fetch_state = QuotaFetchState::Failed("usage returned 429".into());
        let mut loading = mk_windows("personal", false, 10, &[]);
        loading.fetch_state = QuotaFetchState::Loading;
        let empty = mk_windows("personal", false, 10, &[]);
        for preferred in [exhausted, failed, loading, empty] {
            let snap = stacked(vec![
                mk_windows("work", true, 100, &[(H5, 50, QuotaStatus::Ok, Some(2))]),
                preferred,
            ]);
            assert!(matches!(
                decide(&snap, &test_config(0)),
                PolicyDecision::NoOp { .. }
            ));
        }
    }

    #[test]
    fn no_return_to_manual_only_preferred() {
        let mut api = mk_windows("api", false, 1, &[(H5, 0, QuotaStatus::Ok, None)]);
        api.account.extra.insert("manual_only".into(), true.into());
        let snap = stacked(vec![
            mk_windows("work", true, 100, &[(H5, 50, QuotaStatus::Ok, Some(2))]),
            api,
        ]);
        assert!(matches!(
            decide(&snap, &test_config(0)),
            PolicyDecision::NoOp { .. }
        ));
    }

    /// 当前已是最偏好账号时不动，即使低优先级账号余量更多。
    #[test]
    fn preferred_active_never_moves_to_lower_priority() {
        let snap = stacked(vec![
            mk_windows("personal", true, 10, &[(H5, 80, QuotaStatus::Ok, Some(2))]),
            mk_windows("work", false, 100, &[(H5, 0, QuotaStatus::Ok, None)]),
        ]);
        assert!(matches!(
            decide(&snap, &test_config(0)),
            PolicyDecision::NoOp { .. }
        ));
    }

    #[test]
    fn return_picks_most_preferred_account() {
        let snap = stacked(vec![
            mk_windows("work2", true, 100, &[(H5, 10, QuotaStatus::Ok, Some(2))]),
            mk_windows("work1", false, 50, &[(H5, 0, QuotaStatus::Ok, Some(1))]),
            mk_windows("personal", false, 10, &[(H5, 30, QuotaStatus::Ok, Some(4))]),
        ]);
        match decide(&snap, &test_config(0)) {
            PolicyDecision::Swap { to, .. } => assert_eq!(to.0, "personal"),
            other => panic!("expected Swap, got {other:?}"),
        }
    }

    /// 切走时 priority 先于重置时间：直接落到偏好账号，免得下一轮再回切一次。
    #[test]
    fn swap_away_prefers_priority_over_sooner_reset() {
        let snap = stacked(vec![
            mk_windows("work1", true, 100, &[(H5, 99, QuotaStatus::Warn, Some(2))]),
            mk_windows("work2", false, 100, &[(H5, 60, QuotaStatus::Ok, Some(1))]),
            mk_windows(
                "personal",
                false,
                10,
                &[
                    (H5, 20, QuotaStatus::Ok, Some(4)),
                    (D7, 40, QuotaStatus::Ok, Some(100)),
                ],
            ),
        ]);
        match decide(&snap, &test_config(0)) {
            PolicyDecision::Swap { to, .. } => assert_eq!(to.0, "personal"),
            other => panic!("expected Swap, got {other:?}"),
        }
    }

    /// 偏好账号整周耗尽时照常溢出到其他账号，周额度重置后才回切。
    #[test]
    fn exhausted_preferred_overflows_then_returns_after_reset() {
        let blocked = stacked(vec![
            mk_windows(
                "personal",
                true,
                10,
                &[
                    (H5, 30, QuotaStatus::Ok, Some(3)),
                    (D7, 100, QuotaStatus::Exhausted, Some(40)),
                ],
            ),
            mk_windows("work", false, 100, &[(H5, 0, QuotaStatus::Ok, None)]),
        ]);
        match decide(&blocked, &test_config(0)) {
            PolicyDecision::Swap { to, .. } => assert_eq!(to.0, "work"),
            other => panic!("expected Swap, got {other:?}"),
        }

        let reset = stacked(vec![
            mk_windows("work", true, 100, &[(H5, 40, QuotaStatus::Ok, Some(3))]),
            mk_windows(
                "personal",
                false,
                10,
                &[
                    (H5, 0, QuotaStatus::Ok, None),
                    (D7, 0, QuotaStatus::Ok, Some(168)),
                ],
            ),
        ]);
        match decide(&reset, &test_config(0)) {
            PolicyDecision::Swap { to, .. } => assert_eq!(to.0, "personal"),
            other => panic!("expected Swap, got {other:?}"),
        }
    }

    /// usage 端点看不见的限额：只有被拒窗口，5h 仍健康，也要切走且不再作为回切目标。
    #[test]
    fn rejected_account_swaps_away_and_is_not_returned_to() {
        use QuotaWindow::Rejected as RJ;
        let swap_away = stacked(vec![
            mk_windows(
                "work",
                true,
                10,
                &[
                    (H5, 10, QuotaStatus::Ok, Some(2)),
                    (RJ, 100, QuotaStatus::Exhausted, Some(40)),
                ],
            ),
            mk_windows("personal", false, 100, &[(H5, 0, QuotaStatus::Ok, None)]),
        ]);
        match decide(&swap_away, &test_config(0)) {
            PolicyDecision::Swap { to, .. } => assert_eq!(to.0, "personal"),
            other => panic!("expected Swap, got {other:?}"),
        }

        let no_return = stacked(vec![
            mk_windows("personal", true, 100, &[(H5, 10, QuotaStatus::Ok, Some(2))]),
            mk_windows(
                "work",
                false,
                10,
                &[
                    (H5, 10, QuotaStatus::Ok, Some(2)),
                    (RJ, 100, QuotaStatus::Exhausted, Some(40)),
                ],
            ),
        ]);
        assert!(matches!(
            decide(&no_return, &test_config(0)),
            PolicyDecision::NoOp { .. }
        ));
    }

    /// 当前账号 usage 查询失败也不影响：客户端的拒绝本身就是确认。
    #[test]
    fn rejection_decides_even_when_active_quota_fetch_failed() {
        let mut work = mk_windows(
            "work",
            true,
            100,
            &[(QuotaWindow::Rejected, 100, QuotaStatus::Exhausted, Some(40))],
        );
        work.fetch_state = QuotaFetchState::Failed("usage returned 429".into());
        let snap = stacked(vec![
            work,
            mk_windows("personal", false, 100, &[(H5, 0, QuotaStatus::Ok, None)]),
        ]);
        match decide(&snap, &test_config(0)) {
            PolicyDecision::Swap { to, .. } => assert_eq!(to.0, "personal"),
            other => panic!("expected Swap, got {other:?}"),
        }
    }

    /// 全员不可用时，被拒账号按封锁截止时间参与回退排序。
    #[test]
    fn rejected_everywhere_falls_back_to_soonest_unblock() {
        use QuotaWindow::Rejected as RJ;
        let snap = stacked(vec![
            mk_windows(
                "work",
                true,
                100,
                &[
                    (H5, 10, QuotaStatus::Ok, Some(2)),
                    (RJ, 100, QuotaStatus::Exhausted, Some(40)),
                ],
            ),
            mk_windows(
                "personal",
                false,
                100,
                &[(H5, 100, QuotaStatus::Exhausted, Some(3))],
            ),
        ]);
        match decide(&snap, &test_config(0)) {
            PolicyDecision::Swap { to, reason, .. } => {
                assert_eq!(to.0, "personal");
                assert!(reason.contains("recovers soonest"), "{reason}");
            }
            other => panic!("expected Swap, got {other:?}"),
        }
    }

    /// 按模型的周额度耗尽与 7d 一样阻断：切走，且不作为候选。
    #[test]
    fn exhausted_model_week_blocks_like_seven_day() {
        let snap = stacked(vec![
            mk_windows(
                "work",
                true,
                100,
                &[
                    (H5, 10, QuotaStatus::Ok, Some(2)),
                    (MW, 100, QuotaStatus::Exhausted, Some(60)),
                ],
            ),
            mk_windows("personal", false, 100, &[(H5, 0, QuotaStatus::Ok, None)]),
        ]);
        match decide(&snap, &test_config(0)) {
            PolicyDecision::Swap { to, .. } => assert_eq!(to.0, "personal"),
            other => panic!("expected Swap, got {other:?}"),
        }
    }

    #[test]
    fn priority_breaks_tie_when_usage_equal() {
        let mut b = mk_awq("b", false, 10, QuotaStatus::Ok);
        let mut c = mk_awq("c", false, 10, QuotaStatus::Ok);
        b.account.priority = 50;
        c.account.priority = 10;
        let snap = ProviderSnapshot {
            provider: "claude".into(),
            pool_semantics: QuotaPoolSemantics::Stacked,
            accounts: vec![mk_awq("a", true, 99, QuotaStatus::Warn), b, c],
        };
        let d = decide(&snap, &test_config(60_000));
        match d {
            PolicyDecision::Swap { to, .. } => assert_eq!(to.0, "c"),
            other => panic!("expected Swap, got {other:?}"),
        }
    }

    /// 全员耗尽回退（2026-09-30 用户决策，替代旧的「耗尽号之间不按重置时间挑」）：
    /// 无可用候选时，在已确认耗尽的号里选恢复最快的一个（b 3m 胜出 c 1h）。
    #[test]
    fn all_exhausted_picks_soonest_reset_among_depleted() {
        let now = Utc::now();
        let mut active = mk_awq("a", true, 100, QuotaStatus::Exhausted);
        active.quotas = vec![
            mk_quota_with_reset(
                100,
                QuotaStatus::Exhausted,
                Some(now + chrono::Duration::hours(5)),
            ),
            mk_quota_with_reset(80, QuotaStatus::Ok, Some(now + chrono::Duration::hours(46))),
        ];

        let mut sooner = mk_awq("b", false, 100, QuotaStatus::Exhausted);
        sooner.quotas = vec![
            mk_quota_with_reset(
                100,
                QuotaStatus::Exhausted,
                Some(now + chrono::Duration::minutes(3)),
            ),
            mk_quota_with_reset(72, QuotaStatus::Ok, Some(now + chrono::Duration::days(4))),
        ];

        let mut later = mk_awq("c", false, 100, QuotaStatus::Exhausted);
        later.quotas = vec![mk_quota_with_reset(
            100,
            QuotaStatus::Exhausted,
            Some(now + chrono::Duration::hours(1)),
        )];

        let snap = ProviderSnapshot {
            provider: "codex".into(),
            pool_semantics: QuotaPoolSemantics::Stacked,
            accounts: vec![active, later, sooner],
        };
        let d = decide(&snap, &test_config(60_000));
        match d {
            PolicyDecision::Swap { from, to, .. } => {
                assert_eq!(from.unwrap().0, "a");
                assert_eq!(to.0, "b");
            }
            other => panic!("expected fallback swap to soonest recovery, got {other:?}"),
        }
    }

    /// 当前号恢复最快 → 留守 NoOp（旧规则要求 Degraded，已被 2026-09-30 用户决策替代）。
    #[test]
    fn stays_when_active_recovers_soonest() {
        let now = Utc::now();
        let mut active = mk_awq("a", true, 100, QuotaStatus::Exhausted);
        active.quotas = vec![mk_quota_with_reset(
            100,
            QuotaStatus::Exhausted,
            Some(now + chrono::Duration::minutes(3)),
        )];

        let mut later = mk_awq("b", false, 100, QuotaStatus::Exhausted);
        later.quotas = vec![mk_quota_with_reset(
            100,
            QuotaStatus::Exhausted,
            Some(now + chrono::Duration::hours(1)),
        )];

        let snap = ProviderSnapshot {
            provider: "codex".into(),
            pool_semantics: QuotaPoolSemantics::Stacked,
            accounts: vec![active, later],
        };
        let d = decide(&snap, &test_config(60_000));
        assert!(
            matches!(d, PolicyDecision::NoOp { ref reason, .. } if reason.contains("recovers soonest")),
            "got {d:?}"
        );
    }

    fn cursor_account(
        id: &str,
        active: bool,
        first_used: u64,
        first_status: QuotaStatus,
        api_used: u64,
        api_status: QuotaStatus,
        reset_days: i64,
    ) -> AccountWithQuotas {
        let reset_at = Some(Utc::now() + chrono::Duration::days(reset_days));
        let mut account = mk_account(id, active);
        account.provider = "cursor".into();
        AccountWithQuotas {
            account,
            quotas: vec![
                mk_quota_with_window(
                    first_used,
                    first_status,
                    QuotaWindow::FirstPartyModels,
                    reset_at,
                ),
                mk_quota_with_window(api_used, api_status, QuotaWindow::Api, reset_at),
            ],
            fetch_state: QuotaFetchState::Ready,
        }
    }

    /// Cursor API 与官方模型是并行配额。两边 API 都是 0% 时，不能把「1st 还有余量」
    /// 的号和「1st 也耗尽」的号一视同仁，再按 billing cycle 谁先重置就切到全空号。
    #[test]
    fn cursor_api_exhausted_does_not_block_first_party_candidate() {
        let snap = ProviderSnapshot {
            provider: "cursor".into(),
            pool_semantics: QuotaPoolSemantics::Parallel,
            accounts: vec![
                cursor_account(
                    "caleb",
                    true,
                    100,
                    QuotaStatus::Exhausted,
                    100,
                    QuotaStatus::Exhausted,
                    19,
                ),
                cursor_account(
                    "kimberly",
                    false,
                    88,
                    QuotaStatus::Ok,
                    100,
                    QuotaStatus::Exhausted,
                    25,
                ),
            ],
        };
        let d = decide(&snap, &test_config(60_000));
        assert!(
            matches!(d, PolicyDecision::Swap { ref to, .. } if to.0 == "kimberly"),
            "got {d:?}"
        );
    }

    #[test]
    fn cursor_stays_on_first_party_headroom_even_if_api_exhausted() {
        let snap = ProviderSnapshot {
            provider: "cursor".into(),
            pool_semantics: QuotaPoolSemantics::Parallel,
            accounts: vec![
                cursor_account(
                    "kimberly",
                    true,
                    88,
                    QuotaStatus::Ok,
                    100,
                    QuotaStatus::Exhausted,
                    25,
                ),
                cursor_account(
                    "caleb",
                    false,
                    100,
                    QuotaStatus::Exhausted,
                    100,
                    QuotaStatus::Exhausted,
                    19,
                ),
            ],
        };
        let d = decide(&snap, &test_config(60_000));
        assert!(matches!(d, PolicyDecision::NoOp { .. }), "got {d:?}");
    }

    /// 用户 2026-08-21 现场：1st 6% left（已用 94% → Warn）对全空号 19d reset。
    #[test]
    fn cursor_six_percent_first_party_beats_empty_sooner_reset() {
        let snap = ProviderSnapshot {
            provider: "cursor".into(),
            pool_semantics: QuotaPoolSemantics::Parallel,
            accounts: vec![
                cursor_account(
                    "caleb",
                    true,
                    100,
                    QuotaStatus::Exhausted,
                    100,
                    QuotaStatus::Exhausted,
                    19,
                ),
                cursor_account(
                    "kimberly",
                    false,
                    94,
                    QuotaStatus::Warn,
                    100,
                    QuotaStatus::Exhausted,
                    24,
                ),
            ],
        };
        let d = decide(&snap, &test_config(60_000));
        assert!(
            matches!(d, PolicyDecision::Swap { ref to, .. } if to.0 == "kimberly"),
            "got {d:?}"
        );
    }

    /// `quota.warn_pct` 只影响展示着色。1st 已 Warn 但仍有余量时，必须能胜过全空号。
    #[test]
    fn cursor_warn_only_first_party_is_still_a_candidate() {
        let snap = ProviderSnapshot {
            provider: "cursor".into(),
            pool_semantics: QuotaPoolSemantics::Parallel,
            accounts: vec![
                cursor_account(
                    "dead",
                    true,
                    100,
                    QuotaStatus::Exhausted,
                    100,
                    QuotaStatus::Exhausted,
                    19,
                ),
                cursor_account(
                    "warn",
                    false,
                    92,
                    QuotaStatus::Warn,
                    100,
                    QuotaStatus::Exhausted,
                    25,
                ),
            ],
        };
        let d = decide(&snap, &test_config(60_000));
        assert!(
            matches!(d, PolicyDecision::Swap { ref to, .. } if to.0 == "warn"),
            "got {d:?}"
        );
    }

    /// 响应里没有官方模型窗口时，纯 API 账号仍按 API 耗尽切换。
    #[test]
    fn cursor_api_only_account_still_swaps_when_api_exhausted() {
        let reset_at = Some(Utc::now() + chrono::Duration::days(19));
        let mut active = mk_awq("api-dead", true, 100, QuotaStatus::Exhausted);
        active.account.provider = "cursor".into();
        active.quotas = vec![mk_quota_with_window(
            100,
            QuotaStatus::Exhausted,
            QuotaWindow::Api,
            reset_at,
        )];
        let mut candidate = mk_awq("api-ok", false, 10, QuotaStatus::Ok);
        candidate.account.provider = "cursor".into();
        candidate.quotas = vec![mk_quota_with_window(
            10,
            QuotaStatus::Ok,
            QuotaWindow::Api,
            Some(Utc::now() + chrono::Duration::days(25)),
        )];
        let snap = ProviderSnapshot {
            provider: "cursor".into(),
            pool_semantics: QuotaPoolSemantics::Parallel,
            accounts: vec![active, candidate],
        };
        let d = decide(&snap, &test_config(60_000));
        assert!(
            matches!(d, PolicyDecision::Swap { ref to, .. } if to.0 == "api-ok"),
            "got {d:?}"
        );
    }

    fn cursor_account_with_credits(
        id: &str,
        active: bool,
        first: (u64, QuotaStatus),
        api: (u64, QuotaStatus),
        credits: (u64, u64, QuotaStatus),
        reset_days: i64,
    ) -> AccountWithQuotas {
        let mut account = cursor_account(id, active, first.0, first.1, api.0, api.1, reset_days);
        let reset_at = account.quotas[0].reset_at;
        account.quotas.push(Quota {
            provider: "cursor".into(),
            account_id: AccountId(id.into()),
            window: QuotaWindow::Credits,
            used: credits.0,
            limit: credits.1,
            reset_at,
            status: credits.2,
            note: None,
        });
        account
    }

    /// 用户 2026-09-05：全员 1st 见底时必须切到仍有 API 余量的号，不能按重置挑全空号。
    #[test]
    fn cursor_prefers_api_remaining_over_sooner_reset_empty_account() {
        let snap = ProviderSnapshot {
            provider: "cursor".into(),
            pool_semantics: QuotaPoolSemantics::Parallel,
            accounts: vec![
                cursor_account(
                    "kimberly",
                    true,
                    100,
                    QuotaStatus::Exhausted,
                    100,
                    QuotaStatus::Exhausted,
                    9,
                ),
                cursor_account(
                    "terry",
                    false,
                    100,
                    QuotaStatus::Exhausted,
                    100,
                    QuotaStatus::Exhausted,
                    16,
                ),
                cursor_account(
                    "hillard",
                    false,
                    100,
                    QuotaStatus::Exhausted,
                    100,
                    QuotaStatus::Exhausted,
                    25,
                ),
                cursor_account(
                    "kochis",
                    false,
                    100,
                    QuotaStatus::Exhausted,
                    90,
                    QuotaStatus::Ok,
                    25,
                ),
            ],
        };
        let d = decide(&snap, &test_config(60_000));
        assert!(
            matches!(d, PolicyDecision::Swap { ref to, .. } if to.0 == "kochis"),
            "got {d:?}"
        );
    }

    /// Cursor 并行池：Credits 耗尽但 1st 仍有余量时不换号。
    #[test]
    fn cursor_credits_exhausted_keeps_active_when_first_party_ok() {
        let snap = ProviderSnapshot {
            provider: "cursor".into(),
            pool_semantics: QuotaPoolSemantics::Parallel,
            accounts: vec![
                cursor_account_with_credits(
                    "spent-credits",
                    true,
                    (10, QuotaStatus::Ok),
                    (20, QuotaStatus::Ok),
                    (2000, 2000, QuotaStatus::Exhausted),
                    20,
                ),
                cursor_account_with_credits(
                    "fresh",
                    false,
                    (5, QuotaStatus::Ok),
                    (5, QuotaStatus::Ok),
                    (100, 2000, QuotaStatus::Ok),
                    25,
                ),
            ],
        };
        let d = decide(&snap, &test_config(60_000));
        assert!(matches!(d, PolicyDecision::NoOp { .. }), "got {d:?}");
    }

    /// Cursor 并行池：1st 耗尽但 Credits 仍有余量时不换号。
    #[test]
    fn cursor_first_party_exhausted_keeps_active_when_credits_ok() {
        let snap = ProviderSnapshot {
            provider: "cursor".into(),
            pool_semantics: QuotaPoolSemantics::Parallel,
            accounts: vec![
                cursor_account_with_credits(
                    "spent-1st",
                    true,
                    (100, QuotaStatus::Exhausted),
                    (20, QuotaStatus::Ok),
                    (500, 2000, QuotaStatus::Ok),
                    20,
                ),
                cursor_account_with_credits(
                    "fresh",
                    false,
                    (5, QuotaStatus::Ok),
                    (5, QuotaStatus::Ok),
                    (100, 2000, QuotaStatus::Ok),
                    25,
                ),
            ],
        };
        let d = decide(&snap, &test_config(60_000));
        assert!(matches!(d, PolicyDecision::NoOp { .. }), "got {d:?}");
    }

    /// Cursor 并行池：1st 与 Credits 都耗尽、且 API 也耗尽才换号。
    #[test]
    fn cursor_both_gating_pools_exhausted_triggers_swap() {
        let snap = ProviderSnapshot {
            provider: "cursor".into(),
            pool_semantics: QuotaPoolSemantics::Parallel,
            accounts: vec![
                cursor_account_with_credits(
                    "spent",
                    true,
                    (100, QuotaStatus::Exhausted),
                    (100, QuotaStatus::Exhausted),
                    (2000, 2000, QuotaStatus::Exhausted),
                    20,
                ),
                cursor_account_with_credits(
                    "fresh",
                    false,
                    (5, QuotaStatus::Ok),
                    (5, QuotaStatus::Ok),
                    (100, 2000, QuotaStatus::Ok),
                    25,
                ),
            ],
        };
        let d = decide(&snap, &test_config(60_000));
        assert!(
            matches!(d, PolicyDecision::Swap { ref to, .. } if to.0 == "fresh"),
            "got {d:?}"
        );
    }

    /// Credits 未耗尽（仅 Warn）不因长窗口接近阈值提前切。
    #[test]
    fn cursor_credits_warn_does_not_trigger_early_swap() {
        let snap = ProviderSnapshot {
            provider: "cursor".into(),
            pool_semantics: QuotaPoolSemantics::Parallel,
            accounts: vec![
                cursor_account_with_credits(
                    "active",
                    true,
                    (10, QuotaStatus::Ok),
                    (20, QuotaStatus::Ok),
                    (1900, 2000, QuotaStatus::Warn),
                    20,
                ),
                cursor_account_with_credits(
                    "other",
                    false,
                    (5, QuotaStatus::Ok),
                    (5, QuotaStatus::Ok),
                    (100, 2000, QuotaStatus::Ok),
                    25,
                ),
            ],
        };
        let d = decide(&snap, &test_config(60_000));
        assert!(matches!(d, PolicyDecision::NoOp { .. }), "got {d:?}");
    }

    /// 候选仅 Credits 耗尽、1st 仍可用时仍可选；三池都耗尽的候选被跳过。
    #[test]
    fn cursor_candidate_with_only_credits_exhausted_is_still_viable() {
        let snap = ProviderSnapshot {
            provider: "cursor".into(),
            pool_semantics: QuotaPoolSemantics::Parallel,
            accounts: vec![
                cursor_account_with_credits(
                    "spent-active",
                    true,
                    (100, QuotaStatus::Exhausted),
                    (100, QuotaStatus::Exhausted),
                    (2000, 2000, QuotaStatus::Exhausted),
                    10,
                ),
                cursor_account_with_credits(
                    "credits-spent-1st-ok",
                    false,
                    (5, QuotaStatus::Ok),
                    (5, QuotaStatus::Ok),
                    (2000, 2000, QuotaStatus::Exhausted),
                    5,
                ),
                cursor_account_with_credits(
                    "both-spent",
                    false,
                    (100, QuotaStatus::Exhausted),
                    (100, QuotaStatus::Exhausted),
                    (2000, 2000, QuotaStatus::Exhausted),
                    3,
                ),
            ],
        };
        let d = decide(&snap, &test_config(60_000));
        assert!(
            matches!(d, PolicyDecision::Swap { ref to, .. } if to.0 == "credits-spent-1st-ok"),
            "got {d:?}"
        );
    }
}

//! 原生客户端 hook 入口与安装。
//!
//! - `subswap hook claude-stop-failure`（隐藏）：Claude Code `StopFailure` hook 调用。
//!   额度类拒绝时记录被拒账号，再唤醒 daemon 立即重判；本身从不切换账号，
//!   避免与 daemon 并发切换。
//! - `subswap hooks [install|uninstall]`：在 Claude Code 用户设置里安装 / 卸载该 hook。
//!
//! hook 在 `AppContext::build` 之前分发：隔离会话里 `CLAUDE_CONFIG_DIR` 指向隔离目录，
//! 构建上下文会把隔离目录当成全局 Claude 目录做同步。

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use subswap_core::paths::AppPaths;
use subswap_core::rejections::{record_rejection, record_weekly_anchor, Rejection};
use subswap_core::{Account, AccountId, AccountRegistry, AuditEvent, AuditLog};
use subswap_provider_claude::stop_failure::{self, HookInput};

const PROVIDER: &str = "claude";

/// hook 的结果对 Claude Code 不可见（`StopFailure` 忽略退出码），失败只进日志。
pub fn run_claude_stop_failure() -> Result<()> {
    let mut raw = String::new();
    std::io::stdin()
        .take(1024 * 1024)
        .read_to_string(&mut raw)?;
    let input: HookInput = serde_json::from_str(&raw).unwrap_or_default();
    if !input.is_rate_limit() {
        return Ok(());
    }
    let now = Utc::now();
    let scanned = input
        .transcript_path
        .as_deref()
        .and_then(|p| stop_failure::scan_transcript(p, now));
    let rejected_at = scanned.as_ref().map_or(now, |r| r.rejected_at);

    let paths = AppPaths::resolve()?;
    let home = stop_failure::session_claude_home();
    let isolated = home.starts_with(paths.data_dir.join("envs"));
    let Some(email) = stop_failure::signed_in_email(&home)? else {
        tracing::info!("claude rejection ignored: no signed-in account");
        return Ok(());
    };
    let registry = AccountRegistry::from_default_paths()?;
    let audit = AuditLog::from_default_paths()?;
    let Some(account) = registry.find(PROVIDER, &AccountId(email))? else {
        tracing::info!("claude rejection ignored: account not managed by subswap");
        return Ok(());
    };
    if !isolated && !rejected_while_active(&account, rejected_at) {
        audit.append(AuditEvent::err(
            "client_rejected",
            PROVIDER,
            Some(account.id.0.as_str()),
            "ignored: active account changed after the rejection",
        ));
        return Ok(());
    }

    let reset_at = scanned.as_ref().and_then(|r| r.reset_at);
    let kind = scanned.and_then(|r| r.kind);
    if let (Some(reset), Some("seven_day")) = (reset_at, kind.as_deref()) {
        record_weekly_anchor(PROVIDER, &account.id, reset)?;
    }
    record_rejection(
        PROVIDER,
        &account.id,
        Rejection {
            rejected_at,
            reset_at,
            kind,
        },
    )?;
    audit.append(AuditEvent::ok(
        "client_rejected",
        PROVIDER,
        Some(account.id.0.as_str()),
    ));
    if let Err(e) = crate::daemon_spawn::wake_daemon() {
        tracing::warn!(err = %e, "wake daemon after rejection failed");
    }
    Ok(())
}

/// 全局会话的归属判定：拒绝发生时该账号必须已经是 active。
///
/// 账号若在拒绝之后才被激活（`last_used_at` 晚于拒绝时间），说明 daemon / 用户已经切走，
/// 被拒的是上一个账号；记到当前账号上会把一个好号封掉。
fn rejected_while_active(account: &Account, rejected_at: DateTime<Utc>) -> bool {
    account.active && account.last_used_at.map_or(true, |t| t <= rejected_at)
}

/// `subswap hooks [install|uninstall]`。
pub fn run_hooks(action: Option<&str>) -> Result<()> {
    let home = stop_failure::session_claude_home();
    match action {
        None => {
            let state = if stop_failure::hook_installed(&home)? {
                "installed"
            } else {
                "not installed (run `subswap hooks install`)"
            };
            println!("claude StopFailure hook: {state}");
        }
        Some("install") => {
            let command = hook_command()?;
            if stop_failure::install_hook(&home, &command)? {
                println!("claude StopFailure hook installed: {command}");
            } else {
                println!("claude StopFailure hook already installed");
            }
        }
        Some("uninstall") => {
            if stop_failure::uninstall_hook(&home)? {
                println!("claude StopFailure hook removed");
            } else {
                println!("claude StopFailure hook was not installed");
            }
        }
        Some(other) => bail!("unknown argument {other:?}; expected 'install' or 'uninstall'"),
    }
    Ok(())
}

/// 优先写裸命令名：PATH 上的 `subswap` 就是当前二进制时，升级换路径（如 Homebrew Cellar）
/// 不会让 hook 失效；否则写绝对路径。
fn hook_command() -> Result<String> {
    let exe = std::env::current_exe().context("resolve current subswap executable")?;
    let on_path = find_on_path("subswap");
    let program = match on_path {
        Some(p) if same_file(&p, &exe) => "subswap".to_string(),
        _ => exe.to_string_lossy().into_owned(),
    };
    Ok(format!("{program} {}", stop_failure::HOOK_SUBCOMMAND))
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(active: bool, last_used_at: Option<DateTime<Utc>>) -> Account {
        Account {
            provider: PROVIDER.into(),
            id: AccountId("work@example.com".into()),
            label: "work".into(),
            active,
            created_at: Utc::now(),
            last_used_at,
            priority: 100,
            reserve_pct: 0,
            weekly_reset: None,
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn rejection_attributed_only_to_account_active_at_that_time() {
        let t = Utc::now();
        assert!(rejected_while_active(&account(true, None), t));
        assert!(rejected_while_active(
            &account(true, Some(t - chrono::Duration::minutes(5))),
            t
        ));
        // daemon 在拒绝后已切到该账号：拒绝属于上一个账号。
        assert!(!rejected_while_active(
            &account(true, Some(t + chrono::Duration::seconds(3))),
            t
        ));
        assert!(!rejected_while_active(&account(false, None), t));
    }
}

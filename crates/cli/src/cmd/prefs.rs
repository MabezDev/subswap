//! `subswap priority` / `subswap reserve`：查看或设置账号偏好。
//!
//! - priority：数字越小越优先，默认 100。
//! - reserve：每个窗口留给 subswap 之外的余量百分比，默认 0。
//!
//! 只写 registry，不查 quota、不切换；效果交给默认入口 / daemon 的自动切换。

use std::io::{self, IsTerminal};

use anyhow::Result;
use subswap_core::{Account, AuditEvent};

use crate::app::AppContext;
use crate::cmd::resolve_account;

/// 保留余量上限：再高自动切换几乎不会用这个号，多半是输错。
const MAX_RESERVE_PCT: u8 = 90;

/// 返回 `true` 表示改了优先级，调用方应再打印余量表。
pub fn priority(ctx: &AppContext, target: Option<&str>, value: Option<i32>) -> Result<bool> {
    let Some(acc) = target.map(|t| resolve_account(ctx, t)).transpose()? else {
        print_listing(
            ctx,
            "priority <N | id | provider/id> <value>   (lower is preferred)",
        )?;
        return Ok(false);
    };
    let Some(value) = value else {
        println!("{}/{}  priority {}", acc.provider, acc.id, acc.priority);
        return Ok(false);
    };
    ctx.registry.set_priority(&acc.provider, &acc.id, value)?;
    record(ctx, "set_priority", &acc);
    println!("priority {}/{} → {value}", acc.provider, acc.id);
    Ok(true)
}

/// 返回 `true` 表示改了保留余量，调用方应再打印余量表。
pub fn reserve(ctx: &AppContext, target: Option<&str>, pct: Option<u8>) -> Result<bool> {
    let Some(acc) = target.map(|t| resolve_account(ctx, t)).transpose()? else {
        print_listing(
            ctx,
            "reserve <N | id | provider/id> <percent>   (0 = no reserve)",
        )?;
        return Ok(false);
    };
    let Some(pct) = pct else {
        println!("{}/{}  reserve {}%", acc.provider, acc.id, acc.reserve_pct);
        return Ok(false);
    };
    if pct > MAX_RESERVE_PCT {
        anyhow::bail!("reserve must be between 0 and {MAX_RESERVE_PCT}");
    }
    ctx.registry.set_reserve_pct(&acc.provider, &acc.id, pct)?;
    record(ctx, "set_reserve", &acc);
    println!("reserve {}/{} → {pct}%", acc.provider, acc.id);
    Ok(true)
}

fn record(ctx: &AppContext, action: &str, acc: &Account) {
    ctx.audit.append(AuditEvent::ok(
        action,
        &acc.provider,
        Some(acc.id.0.as_str()),
    ));
}

fn print_listing(ctx: &AppContext, usage: &str) -> Result<()> {
    let ordered = ctx.list_ordered()?;
    if ordered.is_empty() {
        println!(
            "No accounts. Sign in to a supported client, then run `subswap login <provider>`."
        );
        return Ok(());
    }
    let color = io::stdout().is_terminal();
    println!("Usage: subswap {usage}");
    println!();
    let width = ordered
        .iter()
        .map(|a| a.provider.len() + 1 + a.id.0.len())
        .max()
        .unwrap_or(0);
    println!("       {:<width$}  priority  reserve", "");
    for (idx, acc) in ordered.iter().enumerate() {
        let star = if acc.active { "*" } else { " " };
        let qualified = format!("{}/{}", acc.provider, acc.id);
        let line = format!(
            "  {star} {:>2} {qualified:<width$}  {:>8}  {:>6}%",
            idx + 1,
            acc.priority,
            acc.reserve_pct
        );
        if color && !acc.active {
            println!("\x1b[2m{line}\x1b[0m");
        } else {
            println!("{line}");
        }
    }
    Ok(())
}

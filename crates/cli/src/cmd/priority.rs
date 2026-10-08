//! `subswap priority [<id|N> [<value>]]`：查看或设置账号优先级（数字越小越优先，默认 100）。
//!
//! 只写 registry，不查 quota、不切换；是否切回偏好账号交给默认入口 / daemon 的自动切换。

use std::io::{self, IsTerminal};

use anyhow::Result;
use subswap_core::AuditEvent;

use crate::app::AppContext;
use crate::cmd::resolve_account;

/// 返回 `true` 表示改了优先级，调用方应再打印余量表。
pub fn run(ctx: &AppContext, target: Option<&str>, value: Option<i32>) -> Result<bool> {
    let Some(input) = target else {
        print_listing(ctx)?;
        return Ok(false);
    };
    let acc = resolve_account(ctx, input)?;
    let Some(value) = value else {
        println!("{}/{}  priority {}", acc.provider, acc.id, acc.priority);
        return Ok(false);
    };
    ctx.registry.set_priority(&acc.provider, &acc.id, value)?;
    ctx.audit.append(AuditEvent::ok(
        "set_priority",
        &acc.provider,
        Some(acc.id.0.as_str()),
    ));
    println!("priority {}/{} → {value}", acc.provider, acc.id);
    Ok(true)
}

fn print_listing(ctx: &AppContext) -> Result<()> {
    let ordered = ctx.list_ordered()?;
    if ordered.is_empty() {
        println!(
            "No accounts. Sign in to a supported client, then run `subswap login <provider>`."
        );
        return Ok(());
    }
    let color = io::stdout().is_terminal();
    println!("Usage: subswap priority <N | id | provider/id> <value>   (lower is preferred)");
    println!();
    let width = ordered
        .iter()
        .map(|a| a.provider.len() + 1 + a.id.0.len())
        .max()
        .unwrap_or(0);
    for (idx, acc) in ordered.iter().enumerate() {
        let star = if acc.active { "*" } else { " " };
        let qualified = format!("{}/{}", acc.provider, acc.id);
        let line = format!(
            "  {star} {:>2} {qualified:<width$}  {}",
            idx + 1,
            acc.priority
        );
        if color && !acc.active {
            println!("\x1b[2m{line}\x1b[0m");
        } else {
            println!("{line}");
        }
    }
    Ok(())
}

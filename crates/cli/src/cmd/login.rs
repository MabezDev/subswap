//! `subswap login <provider>`：调用厂商官方 CLI 走原生登录流程，再 import 到 subswap。
//!
//! 不复刻 OAuth 流程的动机见 docs/design/ARCHITECTURE.md §3.2。

use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use subswap_core::{AuditEvent, Provider};

use crate::app::AppContext;
use crate::cmd::default::print_status_overview;
use crate::render::account_ref;

pub async fn run(
    ctx: &AppContext,
    provider: &str,
    email: Option<String>,
    sso: bool,
    device_auth: bool,
    extra_args: Vec<String>,
    json: bool,
) -> Result<()> {
    match provider {
        "claude" | "anthropic" => {
            if device_auth {
                bail!("--device-auth is only supported for codex login");
            }
            let mut args = vec!["auth".into(), "login".into(), "--claudeai".into()];
            if let Some(email) = email {
                args.push("--email".into());
                args.push(email);
            }
            if sso {
                args.push("--sso".into());
            }
            args.extend(extra_args);
            run_native_login("claude", args).await?;

            let account = ctx
                .claude
                .import_active(None)
                .context("import Claude login")?;
            ctx.registry
                .set_active("claude", &account.id)
                .context("mark Claude login active")?;
            ctx.audit.append(AuditEvent::ok(
                "login",
                "claude",
                Some(account.id.0.as_str()),
            ));
            println!("login → claude/{}", account_ref(&account.id.0));
            if let Err(e) = subswap_core::record_manual_swap("claude") {
                tracing::warn!(err = %e, provider = "claude", "record manual hold failed");
            }
            return finish(ctx, json).await;
        }
        "codex" | "openai" | "chatgpt" => {
            if email.is_some() || sso {
                bail!("--email and --sso are only supported for claude login");
            }
            let mut args = vec!["login".into()];
            if device_auth {
                args.push("--device-auth".into());
            }
            args.extend(extra_args);
            run_native_login("codex", args).await?;

            let account = ctx
                .codex
                .import_active(None)
                .context("import Codex login")?;
            ctx.registry
                .set_active("codex", &account.id)
                .context("mark Codex login active")?;
            ctx.audit.append(AuditEvent::ok(
                "login",
                "codex",
                Some(account.id.0.as_str()),
            ));
            println!("login → codex/{}", account_ref(&account.id.0));
            if let Err(e) = subswap_core::record_manual_swap("codex") {
                tracing::warn!(err = %e, provider = "codex", "record manual hold failed");
            }
            return finish(ctx, json).await;
        }
        "kimi" | "moonshot" => {
            if email.is_some() || sso || device_auth {
                bail!("--email/--sso/--device-auth are not supported for kimi login");
            }
            // Kimi 登录是交互式 TUI：约定用户先在 kimi 里登录好，这里只导入当前登录的凭证。
            let account = ctx
                .kimi
                .import_active(None)
                .context("import Kimi login; run `kimi` and sign in first")?;
            ctx.registry
                .set_active("kimi", &account.id)
                .context("mark Kimi login active")?;
            ctx.audit
                .append(AuditEvent::ok("login", "kimi", Some(account.id.0.as_str())));
            println!("login → kimi/{}", account_ref(&account.id.0));
            if let Err(e) = subswap_core::record_manual_swap("kimi") {
                tracing::warn!(err = %e, provider = "kimi", "record manual hold failed");
            }
            return finish(ctx, json).await;
        }
        "opencode" => {
            if email.is_some() || sso || device_auth {
                bail!("--email/--sso/--device-auth are not supported for opencode login");
            }
            if !extra_args.is_empty() {
                bail!("API keys use `subswap login opencode-api-key -- <key>`");
            }
            let home = ctx.opencode.go_engine().home();
            let signed_in = tokio::task::spawn_blocking(move || {
                subswap_provider_opencode::console::read_console_live(&home)
                    .map(|live| live.is_some())
            })
            .await
            .context("read OpenCode Console login task failed")?
            .context("read OpenCode Console login")?;
            if !signed_in {
                // 未登录：调官方命令走原生登录流程，再导入 Console 账号。
                let major = tokio::task::spawn_blocking(
                    subswap_provider_opencode::console::detect_major_version,
                )
                .await
                .context("detect OpenCode version task failed")?
                .context("detect OpenCode version")?;
                let args = subswap_provider_opencode::console::login_args(major);
                run_native_login("opencode", args).await?;
            }
            let console = ctx.opencode.clone();
            let account = tokio::task::spawn_blocking(move || console.import_console_active(None))
                .await
                .context("import OpenCode Console task failed")?
                .context("import OpenCode Console login")?;
            ctx.audit.append(AuditEvent::ok(
                "login",
                "opencode",
                Some(account.id.0.as_str()),
            ));
            println!("login → opencode/{}", account_ref(&account.id.0));
            if let Err(e) = subswap_core::record_manual_swap("opencode") {
                tracing::warn!(err = %e, provider = "opencode", "record manual hold failed");
            }
            return finish(ctx, json).await;
        }
        "opencode-api-key" | "opencode-go" => {
            if email.is_some() || sso || device_auth {
                bail!("login options are not supported for opencode-api-key");
            }
            if extra_args.len() > 1 {
                bail!("expected one OpenCode API key after `--`");
            }
            let account = if let Some(key) = extra_args
                .first()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
            {
                let blob = subswap_provider_opencode::blob_from_key(key);
                let key_provider = ctx.opencode_api_key.clone();
                let account = tokio::task::spawn_blocking(move || {
                    key_provider.import_raw(blob, None, Some(false))
                })
                .await
                .context("import OpenCode API key task failed")?
                .context("import OpenCode API key")?;
                ctx.opencode_api_key
                    .activate(&account.id)
                    .await
                    .context("select OpenCode API key")?;
                Some(account)
            } else {
                let key_provider = ctx.opencode_api_key.clone();
                let accounts =
                    tokio::task::spawn_blocking(move || key_provider.sync_accounts(None))
                        .await
                        .context("import OpenCode API keys task failed")?
                        .context("import OpenCode API keys from the official client")?;
                if accounts.is_empty() {
                    bail!("no OpenCode API key found; run `opencode auth login opencode-go` or pass a key after `--`");
                }
                accounts.into_iter().find(|a| a.active)
            };
            if let Some(account) = account {
                ctx.audit.append(AuditEvent::ok(
                    "login",
                    "opencode-api-key",
                    Some(account.id.0.as_str()),
                ));
                println!("login → opencode-api-key/{}", account_ref(&account.id.0));
                if let Err(e) = subswap_core::record_manual_swap("opencode-api-key") {
                    tracing::warn!(err = %e, provider = "opencode-api-key", "record manual hold failed");
                }
            } else {
                ctx.audit
                    .append(AuditEvent::ok("login", "opencode-api-key", None));
                println!("imported OpenCode API keys (none active)");
            }
            return finish(ctx, json).await;
        }
        "commandcode" | "command-code" | "cmd" => {
            if email.is_some() || sso || device_auth {
                bail!("--email/--sso/--device-auth are not supported for commandcode login");
            }
            let account = if let Some(key) = extra_args
                .first()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
            {
                let blob = subswap_provider_commandcode::blob_from_key(key);
                let account = ctx
                    .commandcode
                    .import_raw(blob, None, Some(true))
                    .context("import Command Code API key")?;
                ctx.commandcode
                    .activate(&account.id)
                    .await
                    .context("write Command Code key into auth.json")?;
                account
            } else {
                ctx.commandcode.import_active(None).context(
                    "import Command Code login; run `command-code login` or pass the API key after `--`",
                )?
            };
            ctx.registry
                .set_active("commandcode", &account.id)
                .context("mark Command Code login active")?;
            ctx.audit.append(AuditEvent::ok(
                "login",
                "commandcode",
                Some(account.id.0.as_str()),
            ));
            println!("login → commandcode/{}", account_ref(&account.id.0));
            if let Err(e) = subswap_core::record_manual_swap("commandcode") {
                tracing::warn!(err = %e, provider = "commandcode", "record manual hold failed");
            }
            return finish(ctx, json).await;
        }
        "cursor" => {
            if email.is_some() || sso || device_auth || !extra_args.is_empty() {
                bail!("login options are not supported for cursor login");
            }
            let account = ctx
                .cursor
                .import_active(None)
                .await
                .context("import Cursor login; sign in to Cursor first")?;
            ctx.audit.append(AuditEvent::ok(
                "login",
                "cursor",
                Some(account.id.0.as_str()),
            ));
            println!("login → cursor/{}", account_ref(&account.id.0));
            if let Err(e) = subswap_core::record_manual_swap("cursor") {
                tracing::warn!(err = %e, provider = "cursor", "record manual hold failed");
            }
            return finish(ctx, json).await;
        }
        other => {
            bail!("unknown provider: {other} (expected claude, codex, kimi, cursor, opencode, opencode-api-key or commandcode)")
        }
    }
}

async fn finish(ctx: &AppContext, json: bool) -> Result<()> {
    if !json {
        print_status_overview(ctx).await?;
    }
    Ok(())
}

async fn run_native_login(program: &'static str, args: Vec<String>) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        let display = command_display(program, &args);

        // 直接打开控制终端,绕开 tokio runtime / tracing-subscriber 对父进程
        // fd 0/1/2 可能造成的状态污染(非阻塞标志、行缓冲等)。
        // 在没有控制终端的环境下(pipe/no-tty)退回到 Stdio::inherit。
        let (stdin, stdout, stderr) = open_controlling_tty_for_child();

        let status = Command::new(program)
            .args(&args)
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr)
            .status()
            .with_context(|| format!("failed to start `{display}`"))?;
        if !status.success() {
            bail!("native login failed: `{display}` exited with {status}");
        }
        Ok(())
    })
    .await
    .context("native login task failed")?
}

/// 尽量让子进程拿到对控制终端的全新句柄。任何一步失败都安全退回到
/// `Stdio::inherit()`,这样在没有 TTY 的场景(CI / 管道)下行为不变。
fn open_controlling_tty_for_child() -> (Stdio, Stdio, Stdio) {
    let Some(path) = controlling_tty_path() else {
        return (Stdio::inherit(), Stdio::inherit(), Stdio::inherit());
    };
    let open = |read: bool| {
        std::fs::OpenOptions::new()
            .read(read)
            .write(!read)
            .open(&path)
            .ok()
    };
    match (open(true), open(false), open(false)) {
        (Some(i), Some(o), Some(e)) => (Stdio::from(i), Stdio::from(o), Stdio::from(e)),
        _ => (Stdio::inherit(), Stdio::inherit(), Stdio::inherit()),
    }
}

/// 控制终端的设备路径。优先用 fd 0/1/2 所在终端的真实设备(如 `/dev/ttys001`):
/// macOS 的 kqueue 对经 `/dev/tty` 别名打开的 fd 返回 EINVAL,
/// 用 Bun 打包的 Claude Code 监听 stdin 时会直接崩溃。
#[cfg(unix)]
fn controlling_tty_path() -> Option<std::path::PathBuf> {
    if let Some(path) = [0, 1, 2].into_iter().find_map(tty_device_path) {
        return Some(path);
    }
    (!cfg!(target_os = "macos")).then(|| "/dev/tty".into())
}

#[cfg(not(unix))]
fn controlling_tty_path() -> Option<std::path::PathBuf> {
    None
}

/// fd 是终端时返回其真实设备路径,否则 `None`。用 `ttyname_r`,
/// 因为 `ttyname` 的静态缓冲区在 tokio 多线程下不安全。
#[cfg(unix)]
fn tty_device_path(fd: std::os::fd::RawFd) -> Option<std::path::PathBuf> {
    use std::ffi::CStr;
    use std::os::unix::ffi::OsStrExt;

    let mut buf = [0 as libc::c_char; 256];
    // SAFETY: buf 可写且长度如实传入;成功时 ttyname_r 写入以 NUL 结尾的路径。
    if unsafe { libc::ttyname_r(fd, buf.as_mut_ptr(), buf.len()) } != 0 {
        return None;
    }
    // SAFETY: 上面成功返回保证 buf 内是以 NUL 结尾的 C 字符串。
    let name = unsafe { CStr::from_ptr(buf.as_ptr()) };
    Some(std::ffi::OsStr::from_bytes(name.to_bytes()).into())
}

fn command_display(program: &str, args: &[String]) -> String {
    let mut parts = Vec::with_capacity(args.len() + 1);
    parts.push(program.to_string());
    parts.extend(args.iter().map(|arg| shellish_quote(arg)));
    parts.join(" ")
}

fn shellish_quote(value: &str) -> String {
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '='))
    {
        value.to_string()
    } else {
        format!("{value:?}")
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::ffi::{CStr, OsStr};
    use std::fs::{File, OpenOptions};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::{Path, PathBuf};

    /// 开一对伪终端,返回 master 与 slave 设备路径;不依赖测试进程自己有终端。
    fn open_pty() -> (OwnedFd, PathBuf) {
        // SAFETY: 只调用 POSIX pty 接口并逐一检查返回值;ptsname 的静态缓冲区立即拷贝走。
        unsafe {
            let raw = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
            assert!(raw >= 0, "posix_openpt failed");
            let master = OwnedFd::from_raw_fd(raw);
            assert_eq!(libc::grantpt(master.as_raw_fd()), 0, "grantpt failed");
            assert_eq!(libc::unlockpt(master.as_raw_fd()), 0, "unlockpt failed");
            let name = libc::ptsname(master.as_raw_fd());
            assert!(!name.is_null(), "ptsname failed");
            let path = OsStr::from_bytes(CStr::from_ptr(name).to_bytes()).into();
            (master, path)
        }
    }

    /// 以 O_NOCTTY 打开终端设备,避免测试进程把它变成自己的控制终端。
    fn open_tty(path: &Path) -> File {
        OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY)
            .open(path)
            .expect("open pty slave")
    }

    #[test]
    fn tty_device_path_resolves_real_device_not_dev_tty_alias() {
        let (_master, slave_path) = open_pty();
        let slave = open_tty(&slave_path);

        let resolved = tty_device_path(slave.as_raw_fd()).expect("slave is a tty");
        assert_eq!(resolved, slave_path);
        assert_ne!(resolved, Path::new("/dev/tty"));
    }

    #[test]
    fn tty_device_path_is_none_for_non_tty() {
        let file = tempfile::tempfile().expect("tempfile");
        assert_eq!(tty_device_path(file.as_raw_fd()), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn resolved_tty_device_is_accepted_by_kqueue() {
        let (_master, slave_path) = open_pty();
        let slave = open_tty(&slave_path);
        let reopened = open_tty(&tty_device_path(slave.as_raw_fd()).expect("slave is a tty"));

        // SAFETY: kq 由本测试创建并关闭;kevent 只注册一个事件,不取回事件。
        unsafe {
            let kq = libc::kqueue();
            assert!(kq >= 0, "kqueue failed");
            let change = libc::kevent {
                ident: reopened.as_raw_fd() as libc::uintptr_t,
                filter: libc::EVFILT_READ,
                flags: libc::EV_ADD,
                fflags: 0,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            let rc = libc::kevent(kq, &change, 1, std::ptr::null_mut(), 0, std::ptr::null());
            let err = std::io::Error::last_os_error();
            libc::close(kq);
            assert_eq!(rc, 0, "kqueue rejected the tty fd handed to login: {err}");
        }
    }
}

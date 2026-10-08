---
name: subswap
description: >
  Operate subswap, the multi-account switcher for Claude Code, Codex, Kimi, Cursor, OpenCode and
  Command Code. Use when checking AI account quota, swapping the active account, handling a usage
  limit or rate-limit rejection, tuning autoswap (priority, reserve, weekly reset), or running a
  client under a specific account without changing the global one.
---

# subswap

`subswap` keeps a pool of AI-client accounts, shows their quota, and swaps the globally active
account in place. A background daemon (`subswapd`) polls quota and autoswaps when the active
account passes a threshold. Run it as `subswap` (on PATH); check with `subswap --version`.

## Read state

```sh
subswap --json    # accounts + quota snapshot as JSON, with each window's reset_at
subswap           # same data as a numbered table; also runs the autoswap decision
subswap swap      # numbered account list only, no quota query, never swaps
```

- `--json` is the programmatic view. Each account has `id`, `provider`, `label`, `active`,
  `billing`, `fetch_state` and `quotas[]` (`window`, `used`, `limit`, `reset_at`, `status`, `note`).
- Windows: `five_hour`, `seven_day`, and `model_week` (a per-model weekly cap, model named in `note`).
  `status` of `exhausted` means that window is spent until `reset_at`.
- The no-argument entry can autoswap and starts the daemon. When you only need to look, prefer
  `--json` and do not loop on it.
- Numbers in the table (`swap 3`) are the table's global order and can shift when accounts are
  added or removed. Re-list before using a number; prefer `id`, label or `<provider>/<id>`.

## Swap

```sh
subswap swap <id|N|provider/id>
```

- Manual swap never depends on quota queries. It works when the network, quota API or token is
  broken, and it ignores `reserve`. Use it to get off a limited account.
- After success it prints the same quota table as the default entry. A swap is not undone by the
  next quota refresh.
- **Codex**: an already running Codex session keeps the old account. Tell the user to restart
  Codex sessions, or start a new one.
- **Cursor**: swap quits a running Cursor, switches, and reopens it. Warn the user first.
- Custom Claude API accounts (from `add-api`) are `manual_only`: swap to them by hand, and
  autoswap is disabled while one is active. Swap back to an OAuth account to resume autoswap.

## Autoswap controls

```sh
subswap autoswap [on|off]            # no argument prints state
subswap priority [<id|N> [<value>]]  # lower is preferred, default 100
subswap reserve  [<id|N> [<percent>]] # 0-90, share of each window autoswap leaves unused
subswap weekly-reset [<id|N> [<day> [HH:MM]|none]]  # UTC, e.g. `sun 04:00`
subswap hooks [install|uninstall]    # Claude Code StopFailure hook; no argument shows state
```

- Priority: autoswap picks the most preferred usable account and returns to it once it has headroom.
- Reserve: an account counts as exhausted once usage reaches `100 - percent`. Only autoswap
  respects it.
- Weekly reset: used to estimate when a rejected account is usable again when the client gave no
  reset time.
- Hooks: the hook records a Claude Code limit rejection (including limits the usage endpoint cannot
  see) and wakes the daemon to swap away. The hook itself never swaps. `install` edits Claude Code's
  `settings.json`, so ask before running it.
- Setting a value prints the quota table. These commands do not swap by themselves.

## Handling a limit rejection

1. `subswap --json` and find an account that is not `exhausted` in any window.
2. `subswap swap <id>` to it. If none qualifies, report the earliest `reset_at` instead of guessing.
3. If swapping from Codex, remind the user to restart Codex sessions.

## Run under another account without swapping

```sh
subswap run <claude|codex|kimi|commandcode|opencode-api-key> <id|N> [-- <client args>]
subswap shell <id|N>
eval "$(subswap env <provider/id>)"
```

- This leaves the global active account alone, so several terminals can use different accounts.
- Prefer `run` or `shell`: they absorb rotated credentials on exit. `env` holds no lock and absorbs
  nothing, so use it only for short, temporary work.
- Cursor and OpenCode official accounts cannot run isolated.
- Avoid isolating the currently active global account: if another client uses it too, a refresh
  token rotation can invalidate it.

## Add and remove accounts

```sh
subswap login <claude|codex|kimi|cursor|opencode|opencode-api-key|commandcode>
subswap add-api --preset <deepseek|kimi> --api-key "$KEY" --yes
subswap rm <id|N|provider/id>
```

- `login` needs the user: Claude and Codex drive the vendor's own login flow. For Kimi, Cursor and
  Command Code, the user logs in with the client first, then `login` imports it.
- `rm` of the provider's current native login also signs that client out. Confirm with the user
  before removing an active account. A removed account that is still logged in at the client is
  re-imported on the next run.
- Pass secrets through env vars, not literals in commands.

## Rules

- Do not poll quota in a loop or hammer `subswap`. Quota endpoints are rate limited (Anthropic is
  about once a minute per account). subswap caches for about 90 seconds; respect that.
- A quota query that fails or returns 429 does not mean the token is dead. Do not conclude an
  account is broken from one failed fetch.
- Never copy, print or hand-edit credential files or keychain items. All switching goes through
  `subswap`.
- Output is for humans in the table form and JSON with `--json`. Errors print to the terminal; if a
  command is unclear, run `subswap <command> --help` and `subswap doctor`.

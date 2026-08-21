# kwkly

A local Rust daemon that watches your GitHub PRs for new reviewer comments and
spawns headless [Claude Code](https://claude.com/claude-code) runs to propose
fixes — as **uncommitted diffs on your machine** for you to review. It never
commits, never pushes, and never posts to GitHub.

```
poll GitHub ──► new comments on your PR ──► debounce ──► git worktree of the
PR branch ──► headless `claude -p` run ──► PLAN.md + uncommitted diff +
changes.patch in ~/agent-inbox ──► desktop notification ──► you review
```

## Safety model

1. **Read-only GitHub token.** The agent only ever sees a fine-grained PAT
   with read-only permissions. Even a fully prompt-injected agent cannot
   push, comment, edit, or merge — the capability doesn't exist.
2. **Deny rules.** Each run gets `assets/agent-settings.json`: `git commit`,
   `git push`, and all `gh` write subcommands are denied at the harness level.
3. **Scoped filesystem.** The agent's cwd is the task's worktree; the only
   extra writable directory is the task dir (for `PLAN.md` / `REPLY-DRAFT.md`).
4. **Human gate.** Everything the agent produces sits in `~/agent-inbox`
   until you commit/push/reply yourself.

PR comments are untrusted third-party input — treat the layers above as the
defense, not the agent's good behavior.

## Setup

1. **Token** — create a [fine-grained PAT](https://github.com/settings/personal-access-tokens)
   scoped to the repos you watch, with **read-only** Contents, Issues, and
   Pull requests permissions. Export it:

   ```sh
   export KWKLY_GITHUB_TOKEN=github_pat_...
   ```

2. **Config** — `cp config.example.toml config.toml` and set your username
   and repos.

3. **Run the daemon** (foreground in a terminal is fine; a service is optional):

   ```sh
   cargo run --release            # uses ./config.toml
   cargo run --release -- /path/to/config.toml
   ```

   Log verbosity: `RUST_LOG=kwkly=debug cargo run --release`.

## CLI

The daemon is headless; these subcommands are the interactive layer (run them
from any terminal while the daemon runs — they read the same inbox):

```sh
kwkly status     # table of tracked PRs: state, diff ready?, reviewed?
kwkly review     # walk unreviewed finished tasks one at a time
kwkly prune      # delete task dirs for PRs that merged/closed (asks first)
kwkly [daemon]   # the watcher itself
```

`kwkly review` shows each task's `PLAN.md`, then prompts:

| Key | Action |
|---|---|
| `p` / `d` | show PLAN.md / show the diff (git pager) |
| `a` | apply `changes.patch` onto your real checkout (`git apply --3way`) |
| `o` | open interactive `claude` in the worktree to iterate |
| `x` | discard the worktree changes |
| `e` | show the agent's stderr log (for failed tasks) |
| `m` / `s` / `q` | mark reviewed / skip for now / quit |

Only the daemon writes `state.json` — a lock file (`daemon.lock`) enforces one
daemon per `inbox_dir`, so a second instance fails loudly instead of racing.
Run multiple daemons only with separate configs *and* separate `inbox_dir`s.

## Reviewing a task

Each task lands in `~/agent-inbox/<owner>__<repo>/pr-<n>/`:

| File | What it is |
|---|---|
| `PLAN.md` | Per-comment classification + what the agent changed and why |
| `worktree/` | PR branch checkout with the **uncommitted** changes |
| `changes.patch` | The same changes as a patch (absent if no code changes) |
| `REPLY-DRAFT.md` | Drafted replies for question-type comments (you post them) |
| `comments.json` | The comment batch this run responded to |
| `agent-output.json`, `agent-stderr.log` | Run transcript for debugging |

Typical flow: read `PLAN.md`, then `git -C worktree diff`. To iterate, `cd`
into the worktree and run interactive `claude` — it's a normal checkout.
When happy, cherry-pick/apply onto your real checkout (or commit and push
from the worktree yourself). Discard with `git -C worktree checkout .`.

When a PR is merged/closed its state entry is dropped automatically; the
on-disk task dir stays until you run `kwkly prune` (which confirms before
deleting anything, including unreviewed changes).

## Disk usage: what's shared, what isn't

Per repo there is exactly **one clone** (`<inbox>/<owner>__<repo>/clone/`) —
the single git object store. Per-PR checkouts are **git worktrees** of that
clone: they share all git history/objects and only materialize the working
files of the PR branch. Git data is never duplicated.

What *can* grow per worktree is build output (`target/`, `node_modules`, …)
if the agent builds or tests. Two ways to keep that down:

1. **Share the build cache across worktrees** via `agent_env` in config —
   `"{repo_dir}"` expands to the repo's inbox directory:

   ```toml
   [agent_env]
   CARGO_TARGET_DIR = "{repo_dir}/build-cache"   # Rust: one target dir per repo
   ```

   (Cargo handles concurrent builds against a shared target dir with its own
   locking; dependency artifacts — the bulk of the size — are reused across
   worktrees and PRs.)

2. **`kwkly prune`** deletes finished task dirs — including any per-worktree
   build output — once their PRs merge or close.

## Behavior notes

- **First sighting of a PR is baselined** — old comment history isn't replayed;
  only comments arriving after the daemon starts watching are processed.
- **Debounce**: a burst of review comments becomes one agent run
  (`debounce_secs` of quiet required before dispatch).
- **Comments during a run** are queued and trigger a follow-up run in the
  same worktree, building on the previous (still unreviewed) changes.
- Your own comments and `[bot]` accounts are ignored.
- Crash-safe: state is a JSON file; interrupted runs are re-dispatched on
  restart from the persisted `comments.json`.

## Platform support

macOS and Linux. The daemon itself is portable; the only platform-specific
piece is desktop notifications — `osascript` on macOS (built in), `notify-send`
on Linux (install `libnotify` / `libnotify-bin` from your package manager, or
set `notifications = false`).

## Run as a service — macOS (launchd)

launchd is macOS's service manager: it starts the daemon at login, restarts it
if it crashes, and captures logs.

`~/Library/LaunchAgents/com.kwkly.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>com.kwkly</string>
  <key>ProgramArguments</key><array>
    <string>/path/to/kwkly/target/release/kwkly</string>
    <string>/path/to/kwkly/config.toml</string>
  </array>
  <key>EnvironmentVariables</key><dict>
    <key>KWKLY_GITHUB_TOKEN</key><string>github_pat_...</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>/tmp/kwkly.log</string>
  <key>StandardErrorPath</key><string>/tmp/kwkly.log</string>
</dict></plist>
```

Then `launchctl load ~/Library/LaunchAgents/com.kwkly.plist`.
(Better: keep the token out of the plist by wrapping the binary in a small
script that reads it from the macOS Keychain via `security find-generic-password`.)

## Run as a service — Linux (systemd user service)

`~/.config/systemd/user/kwkly.service`:

```ini
[Unit]
Description=kwkly PR-comment agent daemon

[Service]
ExecStart=/path/to/kwkly/target/release/kwkly /path/to/kwkly/config.toml
# Better than an inline token: EnvironmentFile=%h/.config/kwkly/env (chmod 600)
Environment=KWKLY_GITHUB_TOKEN=github_pat_...
Restart=always
RestartSec=5

[Install]
WantedBy=default.target
```

```sh
systemctl --user daemon-reload
systemctl --user enable --now kwkly   # start now + at every login
journalctl --user -u kwkly -f         # follow logs
loginctl enable-linger $USER            # keep running while logged out
```

How the two map:

| launchd | systemd user service |
|---|---|
| `~/Library/LaunchAgents/*.plist` | `~/.config/systemd/user/*.service` |
| `launchctl load` | `systemctl --user enable --now` |
| `KeepAlive` | `Restart=always` |
| `StandardOutPath` | journald (`journalctl --user -u kwkly`) |

## Not yet implemented

- Issue-assignment tasks (currently PR comments only)
- Top-level PR review bodies (the "Approve/Request changes" summary text —
  only inline + conversation comments are watched)
- Pagination past 100 open PRs / 100 new comments per poll
- A `confirm_dispatch` mode (approve each agent run before it starts)
# kwkly

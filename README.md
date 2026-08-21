# trpr - Triage Pull-Request

Address PR review comments with a [Claude Code](https://claude.com/claude-code)
agent — from inside your checkout, on your terms.

```sh
cd ~/Source/miden-node        # your checkout, on the PR branch
trpr                         # or: trpr ~/Source/miden-node
```

trpr figures out which GitHub repo and PR your current branch belongs to,
fetches the PR's **unresolved comments**, and opens a TUI:

1. **Select** — browse the comments (inline review threads + conversation-tab
   comments, bots filtered out).
2. **Instruct** — per comment: `a` = implement as the reviewer stated,
   `e` = write your own instruction ("do this but use a builder instead",
   "just add a TODO"), `x` = ignore.
3. **Run** — `r` launches one Claude Code agent run that implements every
   instructed comment, **editing your checkout in place**. The TUI streams
   the agent's thinking, tool calls, and output live.
4. **Review in your IDE** — when it's done, the changes are ordinary
   uncommitted edits in your working tree. `git diff`, tweak, commit, push —
   all yours. trpr never commits, never pushes, never posts to GitHub.

## Safety model

- **Read-only GitHub token.** The agent only ever sees a fine-grained PAT
  with read-only permissions — it cannot push, comment, or merge via the
  API even if a malicious comment tries to steer it.
- **Deny rules.** Each run carries Claude Code permission rules denying
  `git commit`, `git push`, and every `gh` write subcommand.
- **Your instructions are the authority.** The prompt tells the agent that
  comment bodies are untrusted third-party text and your per-comment
  instructions override them.
- **Human gate.** Everything ends as uncommitted changes you review before
  anything reaches GitHub.

Note: because the agent runs in your real checkout, treat the deny rules +
read-only token as the guardrails and review the diff before pushing — same
discipline as reviewing any contributor's PR.

## Requirements

| Dependency | Used for | Check |
|---|---|---|
| Rust toolchain | building trpr | `cargo --version` |
| [Claude Code](https://claude.com/claude-code) CLI, logged in | the agent runtime | `claude --version` |
| `git` | repo/branch discovery, dirty check | `git --version` |
| `gh` (GitHub CLI) | the *agent* uses it to read PR context | `gh --version` |

Agent runs consume your Claude subscription/API usage like any Claude Code
session. `gh` needs no login — the agent gets the read-only PAT as `GH_TOKEN`.

## Setup

1. Create a [fine-grained PAT](https://github.com/settings/personal-access-tokens)
   with **read-only** Contents, Issues, and Pull requests permissions on the
   repos you work in. For org-owned repos the PAT's *resource owner* must be
   the org (not your user), with the repo granted — and the org must
   allow/approve fine-grained PATs. GitHub returns 404 (not 403) when a
   token can't see a private repo.

   ```sh
   export TRPR_GITHUB_TOKEN=github_pat_...
   ```

2. Build and install:

   ```sh
   cargo install --path .        # installs ~/.cargo/bin/trpr
   ```

Optional env knobs: `TRPR_CLAUDE_BIN` (default `claude`),
`TRPR_MAX_TURNS` (default 60).

## TUI keys

Navigation is vim-flavored: `j`/`k` line movement, `gg`/`G` jump to
top/bottom, `Ctrl-d`/`Ctrl-u` jump by 10 — all acting on whichever pane has
focus (Tab toggles list ↔ detail).

| Phase | Keys |
|---|---|
| Select | Tab toggle focus · `j`/`k` move/scroll · `gg`/`G` top/bottom · `Ctrl-d`/`Ctrl-u` jump · `a` implement-as-stated · `e`/Enter edit instruction · `x` ignore · `r` run · `q` quit |
| Instruction editor | Enter save · Alt+Enter newline · Esc cancel |
| Running | `q` abort (kills the agent) · `j`/`k`, `gg`/`G`, `Ctrl-d`/`Ctrl-u` scroll |
| Done | `q` quit · scroll as above |

If your working tree has uncommitted changes, trpr warns in the header and
asks for confirmation before starting a run (agent edits would mix with
yours in `git diff`).

## Run artifacts

Each run writes to `.trpr/runs/<timestamp>/` inside your repo:

| File | What it is |
|---|---|
| `SUMMARY.md` | Agent-written: what it did per comment, open questions |
| `prompt.md` | The exact prompt the agent was given (your instructions included) |
| `steps.log` | Human-readable step log (same content the TUI streamed) |
| `transcript.jsonl` | Full raw agent event stream |
| `stderr.log` | Claude Code's stderr |

`.trpr/` contains a self-ignoring `.gitignore` (`*`), so it's visible in
your IDE's file tree but never appears in `git status` or your PR diff.

## What "unresolved comments" means

- Inline review threads whose GitHub thread is **not marked resolved**
  (fetched via GraphQL — REST doesn't expose resolved state), including
  outdated ones (marked in the list).
- All conversation-tab comments — GitHub has no resolved concept for them;
  ignore the irrelevant ones with `x`.
- Comments from `[bot]` accounts are hidden.
- Top-level review summary bodies ("Approve"/"Request changes" text) are
  not yet included.

## Development

Tasks are driven by [`just`](https://github.com/casey/just)
(`brew install just`):

```sh
just            # fmt-check + clippy (-D warnings) + tests — the CI gate
just lint       # clippy only
just fmt        # apply formatting
just test       # tests only
just build      # release build
```

## Notes

- The PR is found by matching your current branch against open PRs' head
  branches (works for same-repo and fork-headed PRs). No open PR → error.
- trpr holds no state between invocations: every launch re-fetches the
  PR's comments fresh. Resolve handled threads on GitHub (or re-`x` them)
  to keep the list short.

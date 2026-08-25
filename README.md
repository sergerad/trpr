# trpr - Triage Pull-Request

Address PR review comments with a [Claude Code](https://claude.com/claude-code)
agent — from inside your checkout, on your terms.

```sh
cd ~/Source/miden-node
trpr           # PR-list mode: browse the repo's open PRs, pick one to triage
trpr .         # direct mode: straight to the PR of the checked-out branch
```

<img width="960" height="461" alt="trpr-recording" src="https://github.com/user-attachments/assets/10e9985e-f2cb-426e-b25d-495ae5c176ce" />

## PR-list mode (`trpr`)

A triage inbox for the repo's open PRs:

- **User-aware.** trpr derives your identity from the token — no config.
  Your PRs are shown by default; `m` toggles all.
- **Ordered by newest comment activity.** Your own comments and bots don't
  count as activity, so replying never makes a PR look like news.
- **Seen-based badges.** `● NEW 2h` = someone commented since you last
  opened that PR in trpr. `seen · 2h` = you're caught up.
- **Enter opens a PR**, switching your checkout to its branch when needed
  (safe: trpr requires a clean tree). Opening always re-fetches comments.
- **Jumplist navigation.** `Ctrl-o` (or Esc) returns to the list; `Ctrl-i`
  (or Tab) resumes the view you left, in-progress instructions intact.

## Direct mode (`trpr <path>`)

Skips the list: `trpr .` jumps straight to the PR of the checkout's current
branch, and errors out if there's no open PR.

## The comment view

Either mode lands here — one screen, four steps:

1. **Select** — browse the PR's unresolved comments (inline review threads
   plus conversation tab; bots hidden).
2. **Instruct** — per comment: `a` implement as stated, `e` write your own
   instruction, `x` ignore.
3. **Run** — `r` starts one agent run for everything you instructed. It
   edits your checkout in place, committing **one commit per handled
   comment**, and streams its thinking and tool calls live.
4. **Review, then push** — the commits are on your branch: `git log` them,
   drop or reword what you don't like, push yourself. trpr never pushes and
   never posts to GitHub.

Each commit carries structured trailers naming the comment it addresses:

```
review: rename to select_block_header_commitments_from_block

Addresses: https://github.com/owner/repo/pull/176#discussion_r38264394
Resolution: implemented as stated
```

These trailers are trpr's cross-session memory: next launch, it scans branch
history and badges comments that already have a commit (`✔`), flags ones
with a **new reply since that commit** (`↺`), and feeds the prior commit sha
to the agent when you instruct a follow-up.

## Safety model

- **Read-only GitHub token.** The agent only ever sees a fine-grained PAT
  with read-only permissions — it cannot push, comment, or merge via the
  API even if a malicious comment tries to steer it.
- **Deny rules.** Each run carries Claude Code permission rules denying
  `git push`, history rewriting (`--amend`, `rebase`, `reset`), `git remote`,
  and every `gh` write subcommand. Local commits are allowed — they're
  reversible and publish nothing; *push* is the gate, and it stays yours.
- **Clean tree required.** trpr refuses to start on a dirty working tree, so
  every commit the agent makes is fully attributable to it.
- **Your instructions are the authority.** The prompt tells the agent that
  comment bodies are untrusted third-party text and your per-comment
  instructions override them.
- **Human gate.** Everything ends as local commits you review before
  anything reaches GitHub — you push.

Note: because the agent runs in your real checkout, treat the deny rules +
read-only token as the guardrails and review the commits (messages included —
they're agent output too) before pushing, same discipline as reviewing any
contributor's PR.

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
`TRPR_MAX_TURNS` (default 60), `TRPR_DATA_DIR` (default `~/.trpr`).

## TUI keys

Navigation is vim-flavored: `j`/`k` line movement, `gg`/`G` jump to
top/bottom, `Ctrl-d`/`Ctrl-u` jump by 10 — all acting on whichever pane has
focus (Tab toggles list ↔ detail).

| Phase | Keys |
|---|---|
| PR list | `j`/`k` move · `gg`/`G` top/bottom · `Ctrl-d`/`Ctrl-u` jump · Enter open (switches branch if needed) · `m` toggle your/all PRs · `Ctrl-i`/Tab resume the last-left comment view (state intact) · `r` refresh · `q`/Esc quit |
| Select | Tab toggle focus · `j`/`k` move/scroll · `gg`/`G` top/bottom · `Ctrl-d`/`Ctrl-u` jump · `a` implement-as-stated · `e`/Enter edit instruction · `x` ignore · `r` run · `Ctrl-o`/Esc back to list (list mode) · `q` quit |
| Instruction editor | **modal (vim)**: `i`/`a`/`I`/`A`/`o`/`O` insert · Esc → normal · `h`/`j`/`k`/`l`, `w`/`b`, `0`/`$`, `gg`/`G` motion · `x`, `dd`, `D` delete · `C` change-to-EOL · `yy` yank · `p` paste · `u` undo, `Ctrl-r` redo · `:wq`/`:x`/`:w` save & close · `:q`/`:q!` cancel · Enter is a plain newline in insert mode |
| Running | `q` abort (kills the agent) · `j`/`k`, `gg`/`G`, `Ctrl-d`/`Ctrl-u` scroll |
| Done | Esc back to list (list mode) · `q` quit · scroll as above |

List glyphs: `·` pending · `✓` instructed · `✗` ignored · `✔` committed in
an earlier run · `↺` committed earlier **but new reply since** · `!`
previously ignored but new activity.

## Run artifacts

Each run writes to `~/.trpr/runs/<owner>__<repo>/pr-<n>/<timestamp>/`
(override the base with `TRPR_DATA_DIR`). Nothing is written inside your
checkout, so there's no gitignore to manage and artifacts survive deleting
the checkout. Clean up a merged PR's runs with
`rm -rf ~/.trpr/runs/<owner>__<repo>/pr-<n>`.

| File | What it is |
|---|---|
| `SUMMARY.md` | Agent-written: what it did per comment, open questions |
| `prompt.md` | The exact prompt the agent was given (your instructions included) |
| `steps.log` | Human-readable step log (same content the TUI streamed) |
| `transcript.jsonl` | Full raw agent event stream |
| `stderr.log` | Claude Code's stderr |

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
- Comments are re-fetched fresh every launch. Cross-session memory comes
  from two places: **`Addresses:` trailers in branch commits** (handled
  comments — survives re-clones, visible to reviewers, robust to squashing
  until merge) and **`ignored.json`** in the PR's data dir (ignored
  comments — the one decision that produces no commit). Ignores auto-expire
  when a thread gets new activity, resurfacing as `!`.
- Resolving handled threads on GitHub remains the way to shrink the list
  permanently — trpr can't resolve them for you (read-only token).

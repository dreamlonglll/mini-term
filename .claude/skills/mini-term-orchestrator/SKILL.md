---
name: mini-term-orchestrator
description: Drive other AI coding sessions (Claude/Codex/Grok/opencode/…) from inside this one, via mini-term's orchestrator control CLI — start them in reachable projects, send prompts, wait for them to settle, and read their results. Use when the user asks you to run work in parallel across projects, delegate a task to another agent session, or supervise several sessions at once.
---

# Orchestrating other AI sessions (mini-term)

You are running inside a mini-term pane whose launcher has **"allow orchestration"**
enabled. That grants this pane — and only this pane — a capability token to start and
drive *other* AI sessions, called **orchestrated sessions**.

Check you actually have it before promising the user anything:

```
$env:MINITERM_ORCHESTRATOR_TOKEN   # pwsh — non-empty means you can orchestrate
echo $MINITERM_ORCHESTRATOR_TOKEN  # bash
```

Empty means this pane has no capability. Say so; do not try to work around it.

<!-- cli-location:start -->
<!--
  Maintainers: mini-term embeds this file (include_str!) and writes a rendered copy
  into <project>/.claude/skills/ and <project>/.codex/skills/ whenever an
  orchestrator pane starts in that project. Only the block between the
  cli-location markers is replaced (with the real mt-agent-cli path and the
  per-shell invocation forms); keep both markers. See
  crates/mt-app/src/orchestrator_skill.rs.
-->
The CLI lives next to the mini-term executable, e.g.
`"E:\Program Files\Mini-Term\mt-agent-cli.exe"` (quote it — the path has spaces).
Run `mt-agent-cli --help` once; the exit codes and per-command notes there are
authoritative. Every command prints JSON on success.
<!-- cli-location:end -->

## Commands

| Command | What it does |
|---|---|
| `list-launchers` | Named launchers you may start (id + name only — you never see their commands) |
| `list-projects` | Projects you can reach: your own + others in the same group. `canStartSessions:false` means you cannot start there |
| `start-session --launcher <id> [--project <id>]` | Start an orchestrated session; returns its `paneId` |
| `list-panes` | Your own orchestrated sessions and their status |
| `send --pane <id> (--text <s> \| --stdin)` | Deliver a prompt, exactly as if the user typed it and pressed Enter once |
| `wait --pane <id> [--timeout <s>]` | Block until that session settles |
| `read-transcript --pane <id> [--cursor <seq>]` | Incremental structured transcript (Claude/Codex/Grok only) |
| `read-screen --pane <id> [--lines <n>]` | Tail of the terminal as plain text (works for every session) |

## The rules — these are not suggestions

1. **Never answer for a human.** When `wait` returns `outcome: "attention"`, that
   session is asking its user for approval or for a decision. Report it in your own
   conversation — quote the prompt (`read-screen` gives you the verbatim text) and
   let the user go handle it in that pane. Do **not** `send` it "y", "yes", "1", or
   an empty line. Its status badge is already yellow; the user can see it.
2. **You cannot orchestrate recursively.** Sessions you start never receive a token,
   whatever their launcher is configured with. Don't design plans that assume a
   nested orchestrator.
3. **You can only see and drive what you started.** Any other pane — the user's own
   sessions, another orchestrator's — answers `paneNotFound`. That is not a bug to
   route around.
4. **The concurrency cap is yours to schedule around.** `sessionLimitReached` (exit 4)
   means you are at the limit, not that something failed. Wait for one of yours to
   finish, then start the next. Never retry in a loop.
5. **Reading results is layered.** `read-transcript` is the good one, but only
   Claude / Codex / Grok keep a readable record; anything else returns
   `transcriptUnsupported` — use `read-screen` for those. `sessionUnidentified` means
   no session has been reported for that pane *yet*; wait a moment or use `read-screen`.
6. **Prompts you send are prompts, not keystrokes.** Multi-line text (code blocks
   included) arrives as one paste and is submitted once. Do not split a prompt into
   several `send` calls hoping to build it up line by line.

## Two timing traps — both confirmed on real hardware

**Do not `send` immediately after `start-session`.** The receipt only means the
launch command reached a live terminal; the agent inside may still be booting and
will silently drop anything pasted before it starts reading input. Confirm it is
listening first:

```
mt-agent-cli start-session --launcher claude --project p-frontend   # -> paneId 2
mt-agent-cli read-screen --pane 2 --lines 20    # look for its input prompt
mt-agent-cli send --pane 2 --text "..."         # only now
```

**`wait` alone cannot tell you a turn finished.** Session status has no notion of
"which turn", so a `wait` issued right after `send` will often return the *previous*
turn's `ai-idle` with `waitedMs` near 0. Use the transcript cursor as the fact:

```
# 1. remember where the transcript ends
mt-agent-cli read-transcript --pane 2            # -> nextCursor 4
# 2. send
mt-agent-cli send --pane 2 --text "do the thing"
# 3. loop: wait, then check whether anything new actually landed
mt-agent-cli wait --pane 2 --timeout 90
mt-agent-cli read-transcript --pane 2 --cursor 4  # 0 messages -> not done, wait again
```

For agents without a transcript, compare `read-screen` output instead.

## A normal run

```
mt-agent-cli list-projects                                  # what can I reach
mt-agent-cli start-session --launcher claude --project p-api
mt-agent-cli read-screen --pane 2 --lines 20                # it is listening
mt-agent-cli read-transcript --pane 2                       # nextCursor = N
mt-agent-cli send --pane 2 --stdin < task.md
mt-agent-cli wait --pane 2 --timeout 120
mt-agent-cli read-transcript --pane 2 --cursor N            # its answer
mt-agent-cli list-panes                                     # everyone's status
```

Starting a session does not steal the user's focus and does not switch their active
project — that is deliberate. The user gets one notification saying you started it.
When your own pane closes, the sessions you started keep running; they simply stop
being reachable by anyone.

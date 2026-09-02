---
name: mini-term-orchestrator
description: Drive other AI coding sessions (Claude/Codex/Grok/opencode/…) from inside this one, via mini-term's orchestrator control CLI — start them in reachable projects, hand them work, and have their results pushed back into this conversation. Use when the user asks you to run work in parallel across projects, delegate a task to another agent session, or supervise several sessions at once.
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
| `send --pane <id> (--text <s> \| --stdin)` | Hand over one piece of work; returns a `taskId` |
| `read-screen --pane <id> [--lines <n>]` | *(fallback)* Tail of that terminal as plain text — works for every session |
| `read-transcript --pane <id> [--cursor <seq>]` | *(fallback)* Incremental structured transcript (Claude/Codex/Grok only) |
| `wait --pane <id> [--timeout <s>]` | *(fallback)* Block until that session settles. You normally never need this |

## Results are pushed to you — do not poll for them

After you `send`, **stop and do something else**. The mini-term desktop watches every
session you started and pushes a **report** into this conversation on its own whenever
one of them finishes a turn, stops for a human, exits, or looks like it never picked
up your prompt.

A report arrives as a user message, but **the user did not write it**. Every line of a
report begins with `[mini-term]`. That marker is how you tell the two apart:

- a message starting with `[mini-term]` → a machine report about a session you
  started. Act on it; do not answer it as if a person had said it.
- anything else → your actual user.

A finished turn looks like this:

```text
[mini-term] The following are automatic reports from the mini-term desktop about your orchestrated sessions (1 in total). The user did not say this - one of the sessions you dispatched has made progress. Decide the next step from it; anything a human must handle, tell the user.

[mini-term] orchestrated session #101 · launcher Claude · project p-api · turn ended · cause Stop · turn took 2m13s · tasks involved: t1
New transcript entries (from #0, 2 in total):
[user] run the test suite
[assistant] done, 3 failing
```

A session that needs a human looks like this:

```text
[mini-term] orchestrated session #101 · launcher Claude · project p-api · stopped, waiting for a human · cause PermissionRequest
You must not answer on the user's behalf. Relay the terminal text below to the user, let a human deal with it in that terminal, and only then send more work.
Tail of the terminal screen:
> Allow running rm -rf /tmp/x ?
  1. Yes  2. No
```

Reports are written in mini-term's display language, so the sentences may come in
Chinese instead. The `[mini-term]` prefix, the `·`-separated header fields and their
order are the same either way.

**When they arrive:** only while *you* are idle and not stopped on a prompt of your
own. While you are working, reports pile up and are delivered merged into a single
message once your turn ends. A session may therefore have finished minutes before you
hear about it — that is by design, not a fault.

### Each kind of report, and what you do about it

**`turn ended`** — that session stopped. Read `cause` before you believe anything:

- `cause Stop` — it ended the turn by itself. The transcript excerpt below the header
  is *its own account*. Treat it as a claim, not as a fact: if the outcome matters,
  verify it yourself (run the tests, read the diff, look at the files) before telling
  the user it is done.
- `cause Interrupt` — a human pressed Esc in that pane. Nothing was delivered.
- `cause Stall` — it went silent and the desktop settled it with a fallback. Nothing
  was delivered; `read-screen` that pane to see where it is stuck.

`tasks involved:` lists the `taskId`s from your `send`s that this report covers.

**`stopped, waiting for a human`** — it is sitting on an approval prompt or an
interactive menu. The report already contains the screen text verbatim. Relay that
text to the user and ask them to go handle it in that pane. Do **not** `send` that
session anything — not `y`, not `1`, not an empty line — and do not start polling it;
the next report will tell you when it moved on. Its status badge is already yellow.

**`the AI inside it exited`** — the agent quit; that pane is a plain shell now. Nothing
more will happen there on its own. If you still need the work done, start a new
session; do not `send` into the shell.

**`that terminal has been closed`** — the pane is gone. Any work you handed it that
never came back is lost. Tell the user; do not try to reach it again.

**`task tN may not have been picked up`** — 15 seconds after your write, that session
still had not started working. Nothing failed and nothing is queued. `read-screen`
that pane to see what is actually on it (still booting? a bare shell? an approval
prompt?), then resend the same prompt if it simply is not there.

## The rules — these are not suggestions

1. **Never answer for a human.** A report saying `stopped, waiting for a human`, or a
   `send` refused with `targetAwaitingHuman`, means that session is asking *its user*
   for approval or for a decision. Quote the prompt in your own conversation — the
   report already carries it verbatim, and after a refused `send` you get it with
   `read-screen` — and let the user go handle it in that pane. Do **not** `send` it
   "y", "yes", "1", or an empty line.
2. **A plain-text question is yours to answer.** When a session ends its turn by
   asking you something in prose, that question reaches you inside a `turn ended`
   report — answer it with `send`, the same way you hand out work. Only approval
   prompts and interactive menus (the `stopped, waiting for a human` reports) belong
   to the human. The tell is which report carried the question, not how it is worded.
3. **`targetAwaitingHuman` is a rule, not a failure.** Nothing was written and nothing
   is queued. Tell the user, hand them the prompt text, and go do something else — the
   report that arrives when that session settles is your signal to send again. Never
   retry `send` in a loop.
4. **Your prompt gets a standing footer appended.** Unless the user turned it off in
   mini-term's settings, every prompt you `send` carries a short instruction about how
   to report back (result / files changed / checks run / anything unfinished), inside
   the same paste, before the Enter. Write your prompt as if it were not there and do
   not repeat those instructions yourself.
5. **A report is a session's self-description.** It is assembled from that session's
   own transcript; there are no tool calls, no command output and no exit codes in it.
   "It says it fixed the tests" is not "the tests pass". Verify anything that matters.
6. **You cannot orchestrate recursively.** Sessions you start never receive a token,
   whatever their launcher is configured with. Don't design plans that assume a
   nested orchestrator.
7. **You can only see and drive what you started.** Any other pane — the user's own
   sessions, another orchestrator's — answers `paneNotFound`. That is not a bug to
   route around.
8. **The concurrency cap is yours to schedule around.** `sessionLimitReached` (exit 4)
   means you are at the limit, not that something failed. Wait for one of yours to
   finish, then start the next. Never retry in a loop.
9. **Prompts you send are prompts, not keystrokes.** Multi-line text (code blocks
   included) arrives as one paste and is submitted once. Do not split a prompt into
   several `send` calls hoping to build it up line by line.

## One timing trap — confirmed on real hardware

**Do not `send` immediately after `start-session`.** The receipt only means the
launch command reached a live terminal; the agent inside may still be booting and
will silently drop anything pasted before it starts reading input. Confirm it is
listening first:

```
mt-agent-cli start-session --launcher claude --project p-frontend   # -> paneId 2
mt-agent-cli read-screen --pane 2 --lines 20    # look for its input prompt
mt-agent-cli send --pane 2 --text "..."         # only now
```

(If you send too early anyway, you will get a `task tN may not have been picked up`
report about it 15 seconds later — that is the safety net, not the plan.)

## A normal run

```
mt-agent-cli list-projects                                  # what can I reach
mt-agent-cli start-session --launcher claude --project p-api    # -> paneId 2
mt-agent-cli read-screen --pane 2 --lines 20                # it is listening
mt-agent-cli send --pane 2 --stdin < task.md                # -> taskId t1
```

Then **stop**. Start the next session, get on with your own work, answer the user —
whatever is useful. Do not wait and do not poll. Some time later this shows up in your
conversation by itself:

```text
[mini-term] orchestrated session #2 · launcher Claude · project p-api · turn ended · cause Stop · turn took 4m02s · tasks involved: t1
New transcript entries (from #0, 6 in total):
...
```

and *that* is when you check the work, tell the user, and hand out the next piece.

## When to fall back to pulling

The read commands are still there; they are for the cases a report cannot cover:

- `list-panes` — after a `desktopBusy` receipt (exit 3), to see whether the session
  actually landed before you start a second one.
- `read-screen` — the verbatim text of an approval prompt, a pane that reported
  `may not have been picked up`, or any agent with no transcript.
- `read-transcript` — the full story when a report says it was truncated, or when you
  need messages from before the report you are holding. `transcriptUnsupported` means
  that agent (opencode, pi, custom launchers) keeps no readable record — use
  `read-screen`. `sessionUnidentified` means no session has been reported for that
  pane yet; the binding is never guessed.
- `wait` — only when you truly have nothing else to do. It cannot tell you that *your*
  turn's work finished: session status has no notion of "which turn", so a `wait`
  issued right after `send` often returns the *previous* turn's `ai-idle` with
  `waitedMs` near 0. The report is the fact; `wait` is not.

Starting a session does not steal the user's focus and does not switch their active
project — that is deliberate. The user gets one notification saying you started it.
When your own pane closes, the sessions you started keep running; they simply stop
being reachable by anyone, and any reports still queued for you are discarded.

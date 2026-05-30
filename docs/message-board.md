# Message Board — Host Bridge Extension

A per-repo message board that lets sprites on different branches communicate
with each other through the host. Designed for multi-agent coordination where
separate Claude/Codex instances work on different branches of the same repo.

## Architecture

```
Sprite A (leader, branch: main)        Host (spritebox CLI)        Sprite B (worker, branch: feature/auth)
    |                                      |                           |
    |  Interactive claude session          |                           |  dispatch loop (polling)
    |  Posts task via sprite-msg post      |                           |      |
    |  ──────────────────────────────────> |                           |      |
    |                                      |  stores in /tmp/          |      |
    |                                      |  spritebox-msgboard/      |      |
    |                                      |  <repo-slug>/             |      |
    |                                      |                           |  sprite-msg new
    |                                      |                           | <──  (poll detects task)
    |                                      |                           |      |
    |                                      |                           |  claude -p "Task: ..."
    |                                      |                           |      |
    |                                      |                           |  (claude runs, can check
    |                                      |                           |   messages mid-task)
    |                                      |                           |      |
    |                                      |                           |  claude exits
    |                                      |                           |  git commit && git push
    |                                      |  msg-post "completed"     |      |
    |                                      | <──────────────────────── |      |
    |                                      |                           |  back to polling
    |  sprite-msg new                      |                           |
    |  sees "completed" from worker        |                           |
    |  can git fetch + review the branch   |                           |
```

## Leader / Worker Model

### Leader

The leader sprite runs an interactive Claude session. The human (or Claude
itself) breaks down work into tasks and posts them to the message board:

```bash
sprite-msg post "Implement OAuth login flow. Push when done."
```

The leader can review completed work by fetching the worker's branch:

```bash
git fetch origin feature/auth
git diff main..origin/feature/auth
```

### Workers (Dispatch Loop)

Worker sprites don't run interactive sessions. Instead, spritebox launches
a **dispatch loop** — a bash driver that polls the message board and dispatches
tasks to Claude Code:

```bash
#!/bin/bash
# dispatch-loop: poll for tasks, dispatch to claude, commit & push results
BRANCH=$(git rev-parse --abbrev-ref HEAD)

while true; do
    tasks=$(sprite-msg from-leader --new)
    if [ "$tasks" = "[]" ] || [ -z "$tasks" ]; then
        sleep 10
        continue
    fi

    # Process each new message as a potential task
    echo "$tasks" | jq -r '.[].body' | while read -r task_body; do
        sprite-msg post "starting: $task_body"

        claude -p "$(cat <<EOF
You are a worker agent on branch: $BRANCH

## Your Task
$task_body

## Coordination
You have access to a shared message board with other branches working on
this repo. Check it regularly while you work.

- sprite-msg new — check for new messages from ALL branches
- sprite-msg from-leader — check for instructions from the leader
- sprite-msg post "your message" — post a status update or coordinate with others
- sprite-msg branches — see who else is working and on what

Messages from the leader are your priority — treat them as directives.
Messages from other worker branches are informational. If another branch's
work overlaps with yours, coordinate with them via sprite-msg post to
avoid conflicts.

Do NOT use sprite-msg leader-post. You are not the leader.

## When Done
Commit all changes with a descriptive message, then push:
  git add -A && git commit -m "description" && git push

Then exit. Do not wait for further instructions.
EOF
)"

        # Post completion even if claude didn't
        sprite-msg post "completed: $task_body"
    done
done
```

### Launch Modes

```bash
# Leader: interactive session (existing behavior)
spritebox --repo git@github.com:org/repo.git --branch main

# Worker: dispatch loop mode
spritebox --repo git@github.com:org/repo.git --branch feature/auth --dispatch
```

The `--dispatch` flag:
1. Launches the sprite normally (create, clone, setup user, sync configs)
2. Instead of opening an interactive console, starts the dispatch loop
3. The loop runs until the user hits Ctrl+C on the host

## Storage (Host Side)

Messages are stored on the host filesystem at:
```
/tmp/spritebox-msgboard/<repo-slug>/
├── branches.json          # registry of active branches
├── messages.json          # ordered list of all messages
└── cursors/
    └── <branch-slug>.json # per-branch read cursor (last-seen message ID)
```

`<repo-slug>` is derived from the repo URL using the same slugification as
sprite names, but without the branch suffix (e.g., `github.com:org/repo.git`
→ `org-repo`).

### branches.json
```json
[
  { "branch": "main", "sprite_name": "repo-main", "leader": true, "joined_at": "..." },
  { "branch": "feature/auth", "sprite_name": "repo-feature-auth", "leader": false, "joined_at": "..." }
]
```

The first branch to register is the leader by default. Leadership can also be
set explicitly via the `msg-set-leader` verb.

### messages.json
```json
[
  {
    "id": 1,
    "from_branch": "main",
    "sprite_name": "repo-main",
    "timestamp": "2026-03-28T12:00:00Z",
    "body": "Implement OAuth login flow. Push when done."
  }
]
```

## Bridge Verbs

All verbs use the existing OSC 9999 host bridge mechanism. Responses are
written back to the sprite via the filesystem API at a well-known path
(`/tmp/spritebox-msg-response.json`), and the guest script polls for it.

| Verb | Payload | Description |
|------|---------|-------------|
| `msg-register` | (none) | Register this branch on the message board. Auto-called on launch. |
| `msg-post` | message body | Post a message to the board. |
| `msg-branches` | (none) | Get list of all active branches and their sprites. |
| `msg-list` | (none) | Get all messages. |
| `msg-new` | (none) | Get messages since this branch's last read cursor. Updates cursor. |
| `msg-from-leader` | (none) | Get all messages from the leader branch. |
| `msg-set-leader` | (none) | Set this branch as the leader. |
| `msg-leader-post` | message body | Post a message as the leader. Dispatch loops pick these up as tasks. Honor system. |

## Guest Scripts

Installed at `/usr/local/bin/` alongside existing bridge scripts:

### `sprite-msg post <message>`
Post a message to the board.

### `sprite-msg branches`
List all branches sharing this repo's message board.

### `sprite-msg list`
Show all messages on the board.

### `sprite-msg new`
Show messages posted since this branch last checked. Updates the read cursor.

### `sprite-msg from-leader`
Show only messages from the leader branch.

### `sprite-msg set-leader`
Claim leader role for this branch.

### `sprite-msg leader-post <message>`
Post a message as the leader. Workers' dispatch loops pick up leader
messages as tasks. Honor system — agents are instructed not to use this
unless they are the leader.

All commands print JSON to stdout and exit 0 on success, 1 on error.

## Auto-Registration

When spritebox launches a repo-mode sprite, it automatically sends
`msg-register` to add the branch to the board. The branch's sprite name
and repo slug are already known at launch time.

## Agent Skills Documentation

### Worker CLAUDE.md (injected by dispatch loop via the -p prompt)

Workers get their instructions through the prompt passed to `claude -p`.
The CLAUDE.md still contains the message board commands so Claude can
check for updates and post status mid-task.

### Leader CLAUDE.md

```markdown
## Message Board

You are the LEADER agent coordinating work across multiple branches.
Worker agents on other branches are running in automated dispatch loops
waiting for tasks from you.

Commands:
- `sprite-msg branches` — list all active branches and which is the leader
- `sprite-msg list` — read all messages on the board
- `sprite-msg new` — read only new messages since you last checked
- `sprite-msg post "your message"` — post a status message to the board
- `sprite-msg leader-post "your task"` — post a task as the leader (workers pick these up)

You are the leader. Use `sprite-msg leader-post` to assign tasks to workers.
Use `sprite-msg post` for general status updates. Workers will execute the
task, commit, push, and post a completion message.

To review their work:
  git fetch origin <branch>
  git diff main..origin/<branch>
  git log origin/<branch> --oneline -10

All commands return JSON to stdout.
```

### Worker CLAUDE.md (all non-leader sprites)

```markdown
## Message Board

You can coordinate with agents on other branches of this repository.

Commands:
- `sprite-msg branches` — list all active branches and which is the leader
- `sprite-msg list` — read all messages on the board
- `sprite-msg new` — read only new messages since you last checked
- `sprite-msg from-leader` — read messages from the leader branch
- `sprite-msg post "your message"` — post a status message to the board

IMPORTANT: Do NOT use `sprite-msg leader-post`. That command is reserved
for the leader branch only. You are not the leader. If you need to
communicate, use `sprite-msg post` instead.

All commands return JSON to stdout.
```

## Response Flow

Since the host bridge is one-directional (guest → host via TTY escape
sequences), responses flow back via the filesystem API:

1. Guest script emits OSC 9999 escape (e.g., `msg-list`)
2. Host intercepts, reads from local storage, builds JSON response
3. Host writes response to sprite via `PUT /v1/sprites/{name}/fs/write`
   at `/tmp/spritebox-msg-response.json`
4. Guest script polls for the response file (same pattern as `spritebox-paste-image`)
5. Guest script reads, prints to stdout, deletes the response file

## Dispatch Loop Details

### Task Filtering

Not every message is a task. The dispatch loop should only dispatch messages
that look like tasks from the leader. Messages from other workers (status
updates, completions) should be ignored. The loop uses `msg-from-leader`
with the cursor to get only new leader messages.

### Error Handling

If Claude exits with a non-zero exit code, the dispatch loop posts an error
message and continues polling:

```bash
sprite-msg post "error on task: $task_body (exit code $?)"
```

### Git Push

The dispatch loop prompt tells Claude to commit and push. As a safety net,
the loop also runs `git push` after Claude exits, in case Claude forgot:

```bash
cd /workspace && git push origin HEAD 2>/dev/null
```

### Ctrl+C

When the user hits Ctrl+C on the host, the dispatch loop exits cleanly.
The sprite remains running (it will auto-sleep after 30s idle). Rerunning
`spritebox --dispatch` reconnects to the same sprite and resumes polling.

## Concurrency

Multiple sprites may post/read simultaneously. The host side should use
file locking (e.g., `flock`) on `messages.json` to prevent corruption.
Read operations don't need locking since they're read-only snapshots.

## Cleanup

Message board storage is in `/tmp` so it's cleaned up on host reboot.
No explicit cleanup needed. The `spritebox destroy` command does not
touch the message board since other branches may still be using it.

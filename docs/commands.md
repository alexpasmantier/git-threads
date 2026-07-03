# Command reference

Every `git threads` command, with its options and behavior. For the concepts behind them
(anchors, drafts, the fold, re-anchoring, sync), start with
[how it works](how-it-works.md) and the [deep dives](README.md).

| command | what it does |
|---|---|
| [`init`](#git-threads-init) | one-time setup of a clone: configures fetching of threads data |
| [`comment`](#git-threads-comment) | starts a thread on a commit, a file, or a line range |
| [`reply`](#git-threads-reply) | replies to a thread, or to a specific message in one |
| [`edit`](#git-threads-edit) | replaces the text of one of your messages (appends an edit event) |
| [`delete`](#git-threads-delete) | retracts one of your messages (appends a tombstone) |
| [`resolve`](#git-threads-resolve) | resolves a thread (`--reopen` to reopen) |
| [`discard`](#git-threads-discard) | removes a drafted event before it's shared (`--all` for every draft) |
| [`list`](#git-threads-list) | all threads, with their re-anchor status against your current code |
| [`show`](#git-threads-show) | one thread: its code context (re-anchored) and conversation |
| [`commit`](#git-threads-commit) | seals everything you've drafted into local history, as one batch |
| [`push`](#git-threads-push) | shares your local discussion history (the fetch–union–push loop) |
| [`pull`](#git-threads-pull) | fetches and integrates other people's discussion data |

Two conventions apply everywhere:

- **Any printed ID is a valid handle.** Wherever a command takes a thread, you can pass the
  thread ID *or the ID of any message in it* (as shown by `show`), and unique prefixes
  suffice — `git threads show 8472` works. Ambiguous prefixes are an error, never a guess.
- **Writes are drafts.** `comment`, `reply`, `edit`, `delete`, and `resolve` stage events
  locally. Nothing enters shared history until `commit`, and nothing leaves your machine
  until `push`.

## Setup

### `git threads init`

```
git threads init [--remote <name>]      # default: origin
```

One-time setup for a clone: adds the fetch refspec that makes plain `git fetch` also bring
in threads data (into a tracking ref — never onto your local data), then runs an initial
fetch. Safe to re-run; `pull` and `push` also self-heal this configuration, so a clone that
never ran `init` still works.

## Writing

### `git threads comment`

```
git threads comment [COMMIT] -m <text> [--file <path> [--lines N | N-M] [--side old|new]] [--base <commit>]
```

Starts a new thread. `COMMIT` defaults to `HEAD` — the commit whose *change* is being
discussed. The anchor's granularity follows the flags:

| flags | thread is about |
|---|---|
| none | the whole change |
| `--file` | one file's change |
| `--file --lines 120` or `--lines 120-128` | specific lines (1-based, inclusive) |

`--side old` anchors to the base version of the file (e.g. commenting on deleted lines);
the default `new` is the version after the change. `--base` overrides the diff base — needed
on a root commit, useful to pick a parent of a merge commit or a branch's merge-base.
Line numbers are validated against the actual file.

```console
$ git threads comment --file src/parser.rs --lines 120-128 -m "does this handle empty input?"
drafted thread 84727c6d0f7c21c40ee8768996a20f540d6b1304 (commit and push to share)
```

### `git threads reply`

```
git threads reply <thread-or-message> -m <text>
```

Replies to a thread. Passing a specific message's ID records that message as the reply's
target (`in_reply_to`); passing the thread ID targets the root. Display is flat either way
(spec v1), but the relationship is stored.

### `git threads edit`

```
git threads edit <message> -m <new text>
```

Replaces the text of one of **your** comments or replies. Append-only: this adds an `edit`
event; the original stays in history and the message shows an `(edited)` marker. Editing
someone else's message is rejected (the fold would ignore it anyway), as is editing a
retracted message. Repeated edits chain — each supersedes the previous version.

### `git threads delete`

```
git threads delete <message>
```

Retracts one of **your** comments or replies with a tombstone. The text remains in stored
history (append-only, greppable) but renders as `[retracted]`. A tombstone is final: it wins
over any edit, regardless of timestamps. Same-author rule as `edit`.

### `git threads resolve`

```
git threads resolve <thread-or-message> [--reopen]
```

Marks the thread resolved (or reopens it with `--reopen`). Concurrent resolve/reopen from
different clones reconciles by last-writer-wins at read time.

### `git threads discard`

```
git threads discard <draft-message>
git threads discard --all
```

Removes drafted events before they're ever shared. Discarding a draft thread's root discards
the whole draft thread. Published events can never be discarded — append-only history starts
at `commit`.

## Reading

### `git threads list`

```
git threads list [--at <commit>]        # default: HEAD
```

All threads, newest first: ID, open/resolved state, anchor location, re-anchored placement
against `--at` (`→ path:lines (relocated|fuzzy(f))`, `(outdated)`, or nothing when the anchor
still matches exactly), message and draft counts, and the first line of the root comment.

### `git threads show`

```
git threads show <thread-or-message> [--at <commit>]
```

One thread in full: the anchor and its diff, where the thread lands on `--at` per the
re-anchoring ladder, the code snippet (from the re-anchored location, or from the original
diff when outdated), and the folded conversation — each message with its ID, author,
timestamp, and `(edited)` / `(draft)` / `[retracted]` markers.

## Sharing

The rhythm is git's own: `commit` records locally, `push` shares, `pull` receives.

### `git threads commit`

```
git threads commit
```

Seals **all** drafted events into local threads history as one commit (`threads: N events in
M threads`), however many commands produced them — one review session, one commit. Local
only; works offline. Nothing to commit is a no-op.

### `git threads push`

```
git threads push [--remote <name>]      # default: origin
```

Shares local threads history: fetches the remote's state into the tracking ref, integrates it
(fast-forward or an automatic, conflict-free union merge), then pushes. If a concurrent push
wins the race, it re-fetches and retries until converged. Drafts are never included — you'll
get a reminder if any exist.

### `git threads pull`

```
git threads pull [--remote <name>]      # default: origin
```

The receive side: fetch into the tracking ref and integrate into your local data. Never
touches your drafts, and never loses local events — divergence unions, it doesn't overwrite.

## Reading the data without the CLI

Everything lives on `refs/threads/data` as ordinary git objects:

```console
$ git ls-tree -r --name-only refs/threads/data                       # every anchor and event
$ git show refs/threads/data:threads/<xx>/<thread-id>/anchor.json    # one document
$ git log refs/threads/data                                          # the history of sessions
```

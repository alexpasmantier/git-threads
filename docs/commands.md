# Command reference

Every `git threads` command, with its options and behavior. For the concepts behind them
(anchors, drafts, the fold, re-anchoring, sync), start with
[how it works](how-it-works.md) and the [deep dives](README.md).

| command | what it does |
|---|---|
| [`init`](#git-threads-init) | one-time setup of a clone: configures fetching of threads data |
| [`deinit`](#git-threads-deinit) | removes git-threads from a clone: configuration and all local data |
| [`comment`](#git-threads-comment) | starts a thread on a change: its whole diff, one file of it, or a line range |
| [`reply`](#git-threads-reply) | replies to a thread, or to a specific message in one |
| [`edit`](#git-threads-edit) | replaces the text of one of your messages (appends an edit event) |
| [`delete`](#git-threads-delete) | retracts one of your messages (appends a tombstone) |
| [`resolve`](#git-threads-resolve) | resolves a thread (`--reopen` to reopen) |
| [`discard`](#git-threads-discard) | removes a drafted event before it's shared (`--all` for every draft) |
| [`list`](#git-threads-list) | all threads — or one change's — with their re-anchor status against your current code |
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

This reference is also available as man pages: `git threads mangen <dir>` writes
`git-threads(1)` and one page per subcommand, generated from the CLI definitions. Install
them into a directory on your manpath (e.g. `~/.local/share/man/man1`) and
`git threads --help` — which git routes to `man git-threads` — starts working.

## Setup

### `git threads init`

```
git threads init [--remote <name>]      # default: origin
```

One-time setup for a clone: adds the fetch refspec that makes plain `git fetch` also bring
in threads data (into a tracking ref — never onto your local data), then runs an initial
fetch and integrates what it finds. Safe to re-run; `pull` and `push` also self-heal this
configuration, so a clone that never ran `init` still works.

From then on, no extra command is needed to receive: every `git threads` command starts by
folding freshly fetched tracking-ref data into your local data (always conflict-free, never
touching your drafts), so whatever your normal `git fetch` / `git pull` brought in is
already there. `git threads pull` remains for fetching on demand.

### `git threads deinit`

```
git threads deinit [--force]
```

Completely uninstalls git-threads from your clone — afterwards the repository is as if no
`git threads` command had ever run.

It is the exact inverse of `init`: removes the fetch refspec from every remote and deletes
everything under `refs/threads/` — data, drafts, tracking refs. Only this clone is touched;
the remote's threads data survives, so `init` starts over from it and the discussions come
back. Unshared work is protected: drafts or local events that reached no remote make
`deinit` refuse, with the way out named (`commit`/`push`, `discard --all`, or `--force` to
drop them knowingly).

## Writing

### `git threads comment`

```
git threads comment [TARGET] [FILE] [-m <text>] [--side old|new]
```

Starts a new thread. `TARGET` names the diff being discussed; `FILE` (a path or
`path:lines`) narrows the thread to one file or line range of it. Without `-m`, your
editor opens, resolved the way git resolves it (`GIT_EDITOR`, `core.editor`, `VISUAL`,
`EDITOR`) — the same goes for `reply` and `edit`.

| target | diff being discussed |
|---|---|
| nothing, or a commit | that commit's change (against its first parent; default `HEAD`) |
| `main..topic` | between the two commits, as in `git diff` |
| `main...topic` | `topic` against its merge-base with `main` — the PR shape |

An empty range side means `HEAD` (`main...` discusses your checkout against `main`). On a
merge or root commit, where the first parent is ambiguous or missing, name the base with a
range. As the only argument, `TARGET` may also be a file of HEAD's change, disambiguated
the way git tells revs from paths — a name that is both is an error; write
`comment HEAD <name>` to mean the file.

`FILE` takes the same `path:lines` shape that `list` and `show` print, so locations can be
pasted straight back in. Line numbers (1-based, inclusive) are validated against the
actual file. `--side old` anchors to the base version of the file (e.g. commenting on
deleted lines); the default `new` is the version after the change.

A file or line comment must touch its diff: the file changed in it, and the lines
overlapping a hunk or its three lines of display context. Annotating code *as it stands* —
an audit note, archaeology — is spelled explicitly with an **empty diff**:
`git threads comment HEAD..HEAD src/parser.rs:120` says "about this code", not "about a
change to it".

```console
$ git threads comment src/parser.rs:120-128 -m "does this handle empty input?"
drafted thread 84727c6d0f7c (commit and push to share)

$ git threads comment main...topic src/parser.rs:120-128 -m "same, but on a branch's diff"
drafted thread 662487d5825d (commit and push to share)
```

### `git threads reply`

```
git threads reply <thread-or-message> [-m <text>]
```

Replies to a thread. Passing a specific message's ID records that message as the reply's
target (`in_reply_to`); passing the thread ID targets the root. Display is flat either way
(spec v1), but the relationship is stored.

### `git threads edit`

```
git threads edit <message> [-m <new text>]
```

Replaces the text of one of **your** comments or replies; without `-m`, your editor opens
pre-filled with the current text. Append-only: this adds an `edit`
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
git threads list [TARGET] [FILE] [--at <commit>] [--open | --resolved]
```

All threads, or a filtered view. The positional grammar mirrors `comment`: `TARGET` names a
change (a commit, or a range like `main..topic` / `main...topic`), `FILE` a path within it.
A thread belongs to a change when its anchored head commit is one of the range's commits:
per-commit comments on any commit of a branch, whole-diff comments on its tip, snapshot
notes at those commits. `--open` / `--resolved` keep only that state, so the review
question is one line:

```console
$ git threads list main...topic --open      # what still needs attention on this branch?
```

One difference from `comment`: a lone path filters across **all** changes — the archaeology
view — and matches anchored paths even for files that no longer exist. Directories match by
prefix, and a `path:lines` spec by overlap with the anchored lines; a name that is both a
commit and a path is an error (write `./{name}` to mean the path).

```console
$ git threads list src/parser.rs            # every discussion this file ever had
$ git threads list src/ --resolved          # settled questions under src/
```

Each thread prints newest first, one aligned line: state glyph (`●` open, `✓` resolved), ID,
anchor location, re-anchored placement against `--at` (default `HEAD`;
`→ path:lines (relocated|fuzzy(f))`, `(outdated)`, or nothing when the anchor still matches
exactly), the first line of the root comment, and — when there's more than one message or a
draft — the counts.

### `git threads show`

```
git threads show <thread-or-message> [--at <commit>]
```

One thread in full: the anchor and its diff, where the thread lands on `--at` per the
re-anchoring ladder, the code snippet (from the re-anchored location, or from the original
diff when outdated), and the folded conversation — each message with its ID, author,
date (relative while recent), and `(edited)` / `(draft)` / `[retracted]` markers.

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

The receive side, on demand: fetch into the tracking ref and integrate into your local data.
Never touches your drafts, and never loses local events — divergence unions, it doesn't
overwrite. Rarely needed after `init`: a plain `git fetch` delivers the data and any
`git threads` command integrates it automatically — `pull` is for when you want the fetch
*now*.

## Reading the data without the CLI

Everything lives on `refs/threads/data` as ordinary git objects:

```console
$ git ls-tree -r --name-only refs/threads/data                       # every anchor and event
$ git show refs/threads/data:threads/<xx>/<thread-id>/anchor.json    # one document
$ git log refs/threads/data                                          # the history of sessions
```

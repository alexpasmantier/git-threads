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
| [`move`](#git-threads-move) | re-pins a thread to where its code lives now |
| [`resolve`](#git-threads-resolve) | resolves a thread (`--reopen` to reopen) |
| [`discard`](#git-threads-discard) | removes a drafted event before it's shared (`--all` for every draft) |
| [`list`](#git-threads-list) | all threads — or one change's — with their re-anchor status against your current code |
| [`show`](#git-threads-show) | one thread: its code context (re-anchored) and conversation |
| [`seen`](#git-threads-seen) | marks a thread (or everything) seen without opening it |
| [`status`](#git-threads-status) | what's drafted and what hasn't been pushed |
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

### `git threads move`

```
git threads move <thread-or-message> <file[:lines]> [--at <commit>]
```

Re-pins a thread to where its code lives now — the way out of `outdated`. When the
re-anchoring algorithm can't follow the code (a file split, a rewrite beyond fuzz), a person
can: `move` records a `move` event carrying a fresh anchor at `--at` (default `HEAD`), and
from then on re-anchoring starts there. Works on any thread, not just outdated ones —
including pinning a whole-change discussion down to the code it's really about.

The original anchor is untouched: it remains the record of what was discussed, and `show`
keeps printing it, with a `Moved:` line (and a `moved` decoration) on top. A wrong move is
fixed by another move — latest wins. Moved threads are findable under both their old and
new paths in `list`.

```console
$ git threads show a023381                  # (open, outdated) — the algorithm gave up
$ git threads move a023381 src/lib/list.rs:210-215
drafted move of thread a023381188062 to src/lib/list.rs:210-215 (commit and push to share)
```

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

Both reading commands page like `git log`: the pager is resolved the way git resolves it
(`GIT_PAGER`, `core.pager`, `PAGER`, then `less`), only when stdout is a terminal.
`git --no-pager threads list` and `GIT_PAGER=cat` turn it off.

### `git threads list`

```
git threads list [TARGET] [FILE] [--at <commit>] [--open | --resolved] [--new] [--oneline]
                 [-p | --stat | --json] [-n <num>] [--author <who>] [--grep <text>]
                 [--since <date>] [--until <date>]
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

Without a forge sending notifications, the inbox question — *what's new for me?* — is
answered locally. Each clone keeps a **seen mark** (`refs/threads/seen`, local only, never
shared): messages by others that arrived since you last looked show up as a `N new`
decoration, and `--new` narrows the listing to exactly those threads. Reading a thread with
`show` marks it seen; [`seen`](#git-threads-seen) does it in bulk. Your own messages are
never news to you, and `init` seeds the mark so freshly imported history doesn't flood the
inbox.

```console
$ git threads list --new                    # what did I miss?
```

One difference from `comment`: a lone path filters across **all** changes — the archaeology
view — and matches anchored paths even for files that no longer exist. Directories match by
prefix, and a `path:lines` spec by overlap with the anchored lines; a name that is both a
commit and a path is an error (write `./{name}` to mean the path).

```console
$ git threads list src/parser.rs            # every discussion this file ever had
$ git threads list src/ --resolved          # settled questions under src/
```

Threads print newest first as `git log`-style blocks: a `thread <id>` header with
decorations (`open`/`resolved`, `moved`, message and draft counts when they say
something), `Author:` / `Date:` / `Anchor:` fields, and the root comment indented below.
The `Anchor:` line answers the scanning reader's one question — *where is this thread in
the code I'm looking at* (`--at`, default `HEAD`): an exact placement is just
`path:lines`, an approximate one carries its status (`(relocated)`, `(fuzzy(f))`), and
when nothing matches the original anchor stands, with its diff, marked `(outdated)`. The
full original-versus-current story is [`show`](#git-threads-show)'s job. `--oneline`
compresses each thread to one line — ID, decorations, location, first line of the root —
the way `git log --oneline` does. `-p` / `--patch` appends the change each thread discusses, kept bounded across many
threads: the diff clipped to the hunks overlapping the anchored lines, a diffstat for
whole-change and whole-file threads, and — when there is no diff to show (snapshot
annotations) — the annotated file excerpt, taken from wherever the code sits at `--at`
(the original excerpt when the thread is outdated). `--stat` appends the diffstat for every thread, like
`git log --stat`. Both compose with `--oneline`; the full patch is `show -p`'s job.

The rest of git log's narrowing vocabulary applies to the thread's root comment:
`-n <num>` / `--max-count` stops after that many threads, `--author <who>` keeps threads
whose author matches (case-insensitive substring of name or email), and `--since` /
`--until` bound the date the thread was started (ISO like `2026-07-01`, `yesterday`, or
`"2 weeks ago"`). `--grep <text>` searches what people said: it keeps threads where any
message's current text contains `<text>` (case-insensitive substring, like `--author`;
retracted messages don't match). That makes the knowledge base retrievable:

```console
$ git threads list --grep "sigpipe"          # where did we discuss that?
```

`--json` swaps the rendered blocks for one JSON array — the interface for anything that
isn't a person: editor plugins, agents, CI. Every filter composes with it. Each element
carries the thread's `id`, `resolved` state, its `anchor` (the anchor.json document,
SPEC.md §3, verbatim), the `moved_to` anchor and `moved_by` author when the thread was
re-pinned (null otherwise), the commit it was re-anchored `at`, the resulting `placement`
(`{"kind": "whole-commit" | "located" | "outdated"}`, with `path`/`lines`/`status` and
`fuzz` when located), and its `messages` — folded state, so each message has its current
`body` (null when retracted) plus `edited`/`retracted`/`draft` flags. Raw events stay one
`git show` away; this is the *interpreted* view — the fold and the re-anchoring algorithm
are exactly the parts a consumer shouldn't reimplement.

```console
$ git threads list main...topic --open --json | jq -e 'length == 0' >/dev/null \
    || echo "unresolved threads on this branch"
```

### `git threads show`

```
git threads show <thread-or-message> [--at <commit>] [-p | --stat | --json]
```

One thread in full: its location history, the change being discussed, and the folded
conversation — each message with its ID, author, date, and `(edited)` / `(draft)` /
`[retracted]` markers. The location history is one line per chapter, nothing suppressed:
`Original:` (the anchor: path, lines, and the diff it was made against), `Moved:` (where
and by whom, if the thread was ever [re-pinned](#git-threads-move)), and `Current:` —
where the code sits at `--at` per the re-anchoring algorithm, with its status
(`(exact)` through `(fuzzy(f))`), or `no match` when the thread is outdated.

The change renders bounded by default: line anchors show the diff clipped to their hunks,
whole-change and whole-file anchors show the diffstat (`git log --stat` style), and
snapshot annotations show the annotated file excerpt — taken from wherever the code sits
at `--at`, so it always agrees with the `Current:` line (the original excerpt when the
thread is outdated). `-p` expands to the full patch;
`--stat` forces the diffstat. `--json` emits the thread as one JSON object, the same shape
as `list --json`'s elements.

Messages you haven't read carry a `(new)` marker, and viewing the thread marks it seen —
it drops out of `list --new`. `--json` deliberately does *not* mark anything seen: a
polling tool must not clear your inbox (its output still carries the per-message `new`
flag).

### `git threads seen`

```
git threads seen [thread-or-message]
git threads seen --undo
```

Marks one thread seen without opening it — or, with no argument, everything: inbox zero.
The mark is per clone and never shared.

Marks chain: every mark (a `show`, a `seen`) keeps the previous one as its parent, so
`--undo` rewinds exactly one step — the cure for a fat-fingered bulk `seen`. Undoing the
very first mark returns the clone to "nothing seen yet".

## Sharing

The rhythm is git's own: `commit` records locally, `push` shares, `pull` receives.

### `git threads status`

```
git threads status
```

Where you are in that cycle, the way `git status` answers it for the working tree: every
drafted event (kind, ID, where it goes, the first line of what it says), per remote how
many sealed events haven't been pushed, and how many threads have activity you haven't
seen (`git threads list --new` shows them). All clean prints
`nothing drafted` / `up to date with origin`.

```console
$ git threads status
2 drafted events in 2 threads (git threads commit to seal, discard to drop):
  comment 84727c6d0f7c  src/parser.rs:120-128  does this handle empty input?
  resolve 3f2a1b9c8d7e  thread bebce03de90b
1 event not yet on origin (git threads push to share)
```

Unpushed counts compare your data against the remote's as of its last fetch, like
`git status` against a tracking branch.

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

### `git threads import`

```
git threads import github <number | url> [--remote <name>]      # default: origin
git threads import github --all [--remote <name>]
```

Liberate your review history: a GitHub PR's review threads become ordinary threads in the
repository — original authors (as their stable `noreply` identities), timestamps, reply
chains, resolution state, and anchors rebuilt from the forge's position data, so old
discussions re-anchor onto today's code like any other thread. `--all` walks every PR of
the repository, one publish commit per PR. Fetching goes through the
[`gh` CLI](https://cli.github.com) (its login is the only authentication needed); a URL may
name any repository, not just the remote's.

The mapping is deterministic — event bytes derive only from forge data and the git DAG —
so two clones importing the same PR produce identical events and sync deduplicates them.
Each imported event records its provenance in an `origin` field (forge, node ID, URL), and
events whose origin is already present are skipped: re-importing is a no-op, and re-running
after new replies arrived on GitHub imports just the new ones. Commits under discussion are
fetched via `refs/pull/N/head` (which GitHub keeps after branch deletion) and pinned by the
import commit, so the discussed code survives even the forge losing it. A thread whose code
truly cannot be reconstructed is skipped with a warning, never half-imported.

Review threads only, for now: PR-level conversation comments and review verdicts
(approve / request changes) are not imported.

## Reading the data without the CLI

Everything lives on `refs/threads/data` as ordinary git objects:

```console
$ git ls-tree -r --name-only refs/threads/data                       # every anchor and event
$ git show refs/threads/data:threads/<xx>/<thread-id>/anchor.json    # one document
$ git log refs/threads/data                                          # the history of sessions
```

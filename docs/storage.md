# Deep dive: storage

How thread data lives inside a git repository. Spec: [SPEC.md](../SPEC.md) §5, §9. Code:
[`crates/git-threads/src/store.rs`](../crates/git-threads/src/store.rs).

## One ref, snapshot trees

Everything lives on a single ref, **`refs/threads/data`**. Its tip commit's tree is a full
snapshot of all discussion data:

```
threads/<shard>/<thread-id>/
    anchor.json                    the immutable anchor
    events/<event-id>.json         one file per event, named by its content hash
    attachments/<event-id>/…       (specified for heavyweight payloads; not implemented yet)
```

Design decisions in that little tree:

- **`<shard>`** is the first two hex characters of the thread ID — a 256-way fan-out so no
  directory grows huge. Git tree objects list all entries; at hundreds of thousands of threads
  a flat directory would make every tree write and diff scan a large object. Two-level fan-out
  is the same trick `.git/objects` itself uses.
- **The tip tree is a complete snapshot**, so reading current state needs no history walk:
  `git show refs/threads/data:threads/84/8472…/anchor.json` just works, and third-party tools
  can read everything with stock git plumbing. History on the ref is still useful (audit,
  incremental index updates via tree diffs) but never *required* for correctness.
- **Events are one file each**, not appended to a log file. Files with content-hash names are
  what makes concurrent merging a union (see [sync](sync.md)); a shared append-file would
  reintroduce conflicts.

## Writes

A write is a **batch** — new threads and/or events appended to existing threads — applied as
one commit:

1. Start from the current tip's tree (or the empty tree on first write).
2. Upsert each anchor/event as a blob at its content-addressed path.
3. If the resulting tree is identical to the base tree, stop: the batch was entirely
   duplicates, and the current tip is returned unchanged. Duplicate publishes are free.
4. Otherwise commit the new tree and update the ref **compare-and-swap** style: the update
   names the tip it expects; if another process moved the ref meanwhile, the update fails
   rather than silently discarding the other write. Callers (and the publish retry loop)
   handle the retry.

Validation happens at write time: schema field tables, "a thread root must be a comment",
"appends must target an existing thread". The storage layer stores; it never repairs.

The spec wants one review session batched into **one commit** (`threads: N events in M
threads`). The plumbing supports it — `Batch` carries arbitrarily many threads and appends —
but today's CLI commits per command; proper batching arrives with draft/unpublished comments
(a client-local concern, deliberately outside the shared format).

## Retention parents: discussed commits can't be garbage-collected

The subtle one. An anchor references `diff.head` — the commit that was reviewed. Branches get
deleted, PRs get squashed, histories get rewritten; git eventually deletes commits that
nothing points to. A thread whose code disappeared would still fold and render its metadata,
but re-anchoring and context rendering need the original objects.

So: for each distinct `diff.head` referenced by a batch's *new anchors*, the publish commit
lists that commit as an **extra parent**. Parenthood makes the discussed commit — and its
whole tree, including the anchored blob — *reachable* from `refs/threads/data`. Reachable
objects survive `git gc`, are included in pushes, and arrive with every fetch of the threads
ref, on every machine and on the server.

`diff.base` needs no parent of its own: in both the single-commit case (parent of `head`) and
the merge-base case, it is an ancestor of `head` and reachable through it.

This repository is the live demonstration: its history was rewritten mid-development, every
commit ID changed, reflogs were expired and `git gc --prune=now` run — and the pre-rewrite
commits the early threads discuss are still present, reachable *only* through the threads
ref's retention parents.

One known inefficiency, tracked as an open discussion thread in this repo's own threads data:
a head already reachable from the current tip gets re-listed as a parent anyway (the spec says
MUST per distinct head). Harmless, but the graph would be tidier with a reachability check.

## Why bodies are inline

Event bodies are always inline in the event JSON — never a reference to a separate blob. This
is a reachability argument, not a style preference: **git reachability walks commits and
trees, not JSON payloads.** A blob referenced only by an ID inside a JSON document is invisible
to that walk — `git gc` would prune it and `git push` wouldn't send it. Data loss by design
accident.

Anything heavyweight (logs, patches, images, long-form analysis) belongs in
`attachments/<event-id>/` as ordinary tree entries: reachable, greppable, and lazy-loadable
via partial clone. One representation per field, no inline-or-blob dualism anywhere.

## Scaling envelope

Back-of-envelope from the spec (500k-line repo, 50 contributors, ~5k reviews/year ≈ 125k
events/year): **25–30 MB/year packed** — the same order as the code history itself. JSON with
sorted keys delta-compresses well; git's packfiles are the compression layer, which is why the
format stores plain text and never pre-compresses.

Reads that need an index ("which threads touch this file?") are deliberately *not* the
format's problem: an index is derived data. The intended client pattern is a local index (e.g.
SQLite `path → thread-ids`) built in one scan of the tip tree and updated incrementally from
tree diffs on each fetch — never shared, always rebuildable.

Honest ceiling: monorepo scale (thousands of writers) would need sharded or per-period refs.
The layout permits it; v1 doesn't define it.

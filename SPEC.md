# git-threads — Git-Native Anchored Discussions, Draft v0.1

> Name: **git-threads**. CLI: `git threads`. Ref namespace: `refs/threads/`.

A specification for storing anchored discussions — threaded comments on commits, files, and diff hunks — **inside a git repository**, so that the record of *why the code is the way it is* is portable, forge-independent, and travels with the repo. Code review is the flagship use case, but the format is equally a home for explanatory annotations, archaeology, and agent commentary. Any tool — GitHub/GitLab importers, local review clients, agents — can read and write it with stock git plumbing.

## 1. Goals & non-goals

**Goals**

- Discussion data lives in the repo, distributed by `git push`/`fetch`, owned by whoever owns the repo.
- Works against existing hosting (GitHub, GitLab, plain SSH remotes) with **zero server cooperation**.
- Deterministic: two independent implementations must agree on thread state and on where a comment is displayed (re-anchoring is specified, not heuristic).
- Concurrent writers never see a merge conflict.
- No infrastructure of its own: git is the object store, compression, transport, and integrity layer. Everything else is derivable client-side.

**Non-goals (v1)**

- Review workflow policy (approvals, required reviewers, CI gates).
- Notifications, identity verification, access control — delegated to the transport/host or future extensions.
- Nested (tree-rendered) threads: threads are **flat**.

## 2. Data model

### 2.1 Threads and events

- A **thread** is a flat conversation attached to one **anchor** (§3).
- A thread contains **events**. Events are **append-only**: nothing is ever modified or deleted in place. Current state is a deterministic fold over the events (§2.4).
- Event types (v1):

| type | required fields | meaning |
|---|---|---|
| `comment` | `body` | Root comment; exactly one per thread; carries no anchor (the thread's `anchor.json` does) |
| `reply` | `body`, `in_reply_to` | Reply to any event in the same thread; rendered flat, ordered by timestamp |
| `edit` | `body`, `supersedes` | Replaces the body of a prior `comment`/`reply`/`edit` by the same author |
| `resolve` | `resolved` (bool) | Toggles thread resolution state |
| `delete` | `supersedes` | Tombstone: marks a prior event as retracted (content remains in history) |

Readers MUST ignore unknown fields and MUST preserve (not drop) events of unknown type when re-serializing.

### 2.2 Event schema

Every event is one JSON document:

```json
{
  "v": 1,
  "type": "reply",
  "author": { "name": "Alex Pasmant", "email": "alex.pasmant@gmail.com" },
  "ts": "2026-07-03T14:12:09Z",
  "in_reply_to": "<event-id>",
  "body": "Right, but this breaks when the hunk header is empty."
}
```

- `ts`: ISO 8601, UTC, second precision. Used for ordering and last-writer-wins.
- `author`: same semantics as git commit authorship. Optional `sig` field is reserved for future signing (§ideas).
- `body`: UTF-8 Markdown (CommonMark). Always inline — never a blob reference (§5.3).

### 2.3 Identifiers

- **Event ID** = lowercase hex SHA-256 of the event's canonical JSON serialization (§6), truncated to 40 characters. The event's filename is its ID. Content addressing makes merges idempotent: the same event always lands at the same path.
- **Thread ID** = the event ID of its root `comment`.

### 2.4 State fold (normative)

Given a thread's event set:

1. **Body** of each comment/reply = the latest `edit` in its supersede-chain, ordered by (`ts`, event ID as tie-break). A `delete` tombstone wins over edits regardless of order and marks the event retracted.
2. **Resolved state** = the `resolved` value of the latest `resolve` event by (`ts`, event ID). Default: unresolved.
3. **Display order** of a thread = events ordered by (`ts`, event ID), rendered flat.
4. Events whose `in_reply_to`/`supersedes` target is absent are still valid (the target may arrive later); render them at the thread level.

## 3. Anchors

Each thread directory contains one immutable `anchor.json`:

```json
{
  "v": 1,
  "kind": "range",
  "diff": { "base": "<commit-sha>", "head": "<commit-sha>" },
  "path": "src/parser.rs",
  "side": "new",
  "lines": { "start": 120, "end": 128 },
  "blob": "<blob-sha>"
}
```

### 3.1 Fields

- `kind`: `commit` (whole-change comment; no `path`/`lines`), `file` (per-file; no `lines`), or `range`.
- `diff.base` / `diff.head`: the two commits whose diff the comment was made against. For a single-commit thread, `base` is the (chosen) parent of `head` — this disambiguates merge commits. For a branch-level discussion, `base` is the merge-base **at the time of commenting**, pinning exactly what the author saw.
- `path`: the file path on `side`. `old_path` MUST be present iff the file was renamed within this diff.
- `side`: `old` (base version — e.g. deleted lines) or `new`. Comments on unchanged context lines use `new`.
- `lines`: 1-based, inclusive, **file coordinates on `side`** — never diff/patch positions.
- `blob`: SHA of the file version on `side`. Redundant by construction; serves as an integrity check. If resolving `head:path` yields a different blob, readers MUST flag the anchor instead of rendering it.
- `cols` (optional, reserved): `{ "start": n, "end": n }` sub-line span; clients MAY ignore.

Anchors are always valid against their own `diff` — they never "break." Displaying a thread on any *other* commit is re-anchoring (§4), which is computed, never stored.

## 4. Re-anchoring (normative)

Purpose: given an anchor `A` and a target commit `T` (e.g. current branch tip), decide where — if anywhere — to display the thread.

### 4.1 The snippet (derived, never stored)

`snippet(A)` = from blob `A.blob`: the `lines` range (**target**), plus 3 lines of context **before** and **after** (fewer at file boundaries). For ranges longer than 20 lines, target = first 10 + last 10 lines, plus SHA-256 of the full range. Exporters materialize snippets at the boundary for consumers without git object access (§8).

### 4.2 The ladder

Evaluate in order; stop at the first success. At every step, a match MUST be **unique** among candidates — two matches means failure of that step, never pick-first.

1. **Blob identity.** If `A.blob` exists in `T`'s tree at `A.path` (or at a rename-detected path, per `git diff -M` between `A.diff.head` and `T`), map lines 1:1. Status: `exact`.
2. **Exact snippet match.** Search the candidate file (same path, then rename-detected path) for `before + target + after` verbatim. Status: `relocated`.
3. **Fuzzed match.** Retry with fuzz level *f* = 1, 2, 3: drop *f* outer context lines from each of `before`/`after` (semantics of `git apply` fuzz). Then retry levels 0–3 with trailing-whitespace-insensitive line comparison. Status: `fuzzy(f)`.
4. **Outdated.** No unique match. Render the thread against its canonical `A.diff.head` using the derived snippet. Status: `outdated`.

Candidate files in v1 are limited to `A.path` plus rename-detected paths. Cross-file search is an explicitly non-normative client extension (§ideas).

The ladder as written applies to `range` anchors. `commit` anchors are never re-anchored — they describe a whole change, which exists only in its own diff. `file` anchors use presence, not content: identical blob at a candidate path → `exact`; a candidate path present with different content → `relocated`; otherwise `outdated`.

Two consequences implementations may rely on: raising the fuzz level only relaxes the pattern, so ambiguity at one level implies ambiguity at every later level — the search may stop at the first ambiguous level. And a truncated snippet's middle is unconstrained by line matching, so a byte-exact match MUST be confirmed against the stored range hash; the whitespace-insensitive pass cannot be, which is one reason its results are `fuzzy(f)`, never `relocated`.

Re-anchor results are pure functions of `(A, T)` — both immutable — so clients SHOULD cache them locally (§7). Caches are never part of the shared format.

## 5. Storage layout

### 5.1 Ref and tree

All data lives on a single ref: **`refs/threads/data`**. Its tip commit's tree is a full snapshot:

```
threads/<shard>/<thread-id>/
    anchor.json
    events/<event-id>.json
    attachments/<event-id>/<filename>      (optional)
```

- `<shard>` = first 2 hex characters of the thread ID (256-way fan-out; keeps directory trees small at hundreds of thousands of threads).
- Current state is entirely readable from the tip tree with stock git (`git show refs/threads/data:threads/...`). No history walk required.

### 5.2 Commits on the threads ref

- A publish operation (e.g. one review session: N comments) SHOULD be batched into **one commit** carrying all its event files.
- **Anchored-commit retention:** for each distinct `diff.head` referenced by newly added anchors, the publish commit MUST list that commit as an **additional parent**. Reachability from `refs/threads/data` then keeps discussed commits alive in every clone that fetches threads — surviving branch deletion, squash merges, and server GC. (`diff.base` needs no parent: it is an ancestor of `head` in both the commit-diff and merge-base cases.)
- Commit messages are informative only; suggested: `threads: <n> events in <m> threads`.
- History on this ref is append-only. Writers MUST NOT force-push it.

### 5.3 Bodies and attachments

- Event bodies are **always inline** in the event JSON. Rationale: median bodies are small; a separate blob referenced only from JSON would be *unreachable* (git reachability walks trees, not payloads) and thus pruned by GC and skipped by push.
- Anything heavyweight (long-form analysis, logs, patches, images) goes in `attachments/<event-id>/` as ordinary tree entries — reachable, greppable, lazy-loadable via partial clone. Events reference attachments by relative filename. No dual inline-or-blob representation exists for any field.

## 6. Canonical JSON serialization (normative)

Required for content-addressed event IDs and for delta-friendly storage:

- UTF-8, no BOM. LF line endings n/a (single line).
- Object keys sorted lexicographically (byte order). No insignificant whitespace.
- Strings: minimal JSON escaping. Numbers: integers only, no floats, no leading zeros.
- The serialized bytes are exactly what is stored and exactly what is hashed for the event ID.

Do not pre-compress or binary-encode anything: git packfiles (zlib + delta chains over structurally similar JSON) are the compression layer, and plain text keeps five years of discussion history `git grep`-able.

## 7. Synchronization

### 7.1 Configuration (per clone, written by `init`)

`init` adds a single fetch refspec to the shared remote:

```
fetch = +refs/threads/data:refs/threads/remotes/<remote>/data
```

Fetch refspecs are additive, so this augments normal fetches. Two deliberate asymmetries:

- **Remote state lands in a tracking ref, never directly on `refs/threads/data`.** A direct `+refs/threads/*:refs/threads/*` mapping would let any fetch force-overwrite the local ref, making unpublished local events unreachable. Integration into the local ref is an explicit step (§7.2): fast-forward when possible, union merge otherwise — exactly git's own branch/tracking-ref model. The tracking ref lives under the format's own namespace (not `refs/remotes/*`, which a branch named `threads/data` could collide with).
- **No push refspec is configured**: setting `remote.<name>.push` *replaces* git's default push behavior for the clone (a bare `git push` would stop pushing the current branch). Publishing instead pushes explicitly — `git push <remote> refs/threads/data:refs/threads/data` — as part of the publish loop (§7.2).

Tools MUST self-heal this configuration (git config and hooks are per-clone by design; "install the tool, run any command once" is the setup floor — the git-lfs pattern).

### 7.2 Publish loop

1. **Fetch** the remote's `refs/threads/data` into the tracking ref (§7.1).
2. **Integrate** the tracking ref into the local `refs/threads/data`: no-op if already contained, fast-forward if the local ref is an ancestor, otherwise a **tree union** merge — the union of both tip trees, committed with both tips as parents. Local ref updates are compare-and-swap on the expected tip.
3. **Push** the local ref explicitly. On non-fast-forward rejection (a concurrent publish won the race), go to 1.

Because writers only ever add files with content-addressed unique names, the union has **no conflict case**; the loop always converges. Concurrent `resolve` toggles reconcile via the state fold (§2.4), not at merge time.

### 7.3 Hosting

GitHub, GitLab, and plain git servers accept pushes to `refs/threads/*` and simply don't render it. Branch protection does not normally cover this namespace. Forks and default clones do not copy it (data arrives on `init`'s first fetch).

## 8. Interoperability

- **Export** (GitHub/GitLab API, static HTML, email): materialize derived snippets (§4.1) so consumers without git object access can render context. The GitHub review-thread model (root comment at a diff position, `in_reply_to` replies, resolved bit) maps ~1:1 onto this format.
- **Import**: foreign comments become threads with anchors reconstructed from the forge's position data; foreign identity maps into `author`; foreign IDs SHOULD be preserved in an `origin` field (`{"forge": "github", "id": "...", "url": "..."}`).
- Round-trip sync (bidirectional PR mirroring) is out of scope for v1; see ideas.

## 9. Scaling notes (informative)

Back-of-envelope for a 500k-line repo, 50 contributors, ~5,000 reviews/year (~125k events):

- ~25–30 MB/year packed (blobs + commits + tree deltas) — same order as the code history itself. Five years ≈ 150 MB.
- Ref contention: batched publishes ≈ 8/hour peak; the retry loop absorbs 10× that without coordination.
- Reads ("which threads touch this file?") require an index — deliberately **not** part of the format (derived data). Clients build a local index (e.g. SQLite: path → thread IDs) in one full scan, then update incrementally from tree diffs per fetch.
- Loose-object accumulation is handled by git auto-gc at these volumes; a `maintenance` command is optional hygiene.
- Honest ceiling: monorepo scale (thousands of contributors) would need sharded/per-period refs — the layout permits it, v1 does not define it.

## 10. Versioning

- Every document carries `"v"`. Readers MUST accept documents with unknown fields and SHOULD surface (not crash on) unknown major versions.
- Additive changes (new optional fields, new event types) do not bump `v`. Semantic changes to the fold, the ladder, or canonical serialization do.

---

## Ideas for later

Format & semantics

- **Suggested changes**: patch-carrying events (GitHub "suggestion" blocks) applied with `git apply`; attachments already give them a reachable home.
- **Review verdicts**: approve / request-changes events; per-review summary rollup.
- **Patchset tracking** (Gerrit-style): link successive heads of a rebased series (via `range-diff`) so threads follow a PR across force-pushes.
- **Reactions** (👍 events), **labels/tags** on threads.
- **Sub-line anchors**: activate the reserved `cols` field (agents want exact spans).
- **Cross-file re-anchoring**: content search beyond the anchored path (survives file splits); high false-positive risk, spec as an interactive client extension with distinct status.
- **Signing**: `sig` on events (SSH/GPG, same trust model as commit signing) for authenticity in low-trust repos.

Storage & scale

- **Sharded refs** (per-shard or per-quarter) past ~10× the v1 scaling envelope.
- **`refs/notes/threads` bridge**: derived per-commit summaries ("3 threads, 1 unresolved") so vanilla `git log --notes=threads` shows discussion presence.
- **Maintenance command**: repack, commit-graph, index rebuild.

Tooling

- **CLI** (`git threads comment|show|resolve|search|export|import|init`) — the single-player wedge.
- **GitHub/GitLab importers** ("liberate your review history"), then bidirectional PR sync.
- **Static HTML export** of a discussion for repo-less readers.
- **Desktop review client**: syntax highlighting, LSP navigation, search — the niceties web review UIs lack.
- **Agent integration**: discuss changes with an agent whose commentary persists as threads; conventions for agent long-form output (attachments, soft body-size cap — agent verbosity is the assumption most likely to break the storage math).
- **Notification bridge**: polling fetch, webhook adapter, or email digest — how a colleague learns there's something new.
- **Client-local niceties** (explicitly outside the shared format): draft/unpublished comments, read-unread tracking, re-anchor cache.

Open questions

- Second-precision timestamps make same-second updates order-undefined beyond the deterministic ID tie-break (e.g. a resolve→reopen toggle within one second, or a reply displaying before its root). Convergence is unaffected; causal intuition is. Millisecond precision, or a per-writer logical counter?
- Threading model for `edit` by non-authors (moderation?).
- Anchors into submodules.
- Very large monorepo path indexing.
- Privacy: thread data is world-readable to anyone with repo access; is per-thread encryption ever worth the complexity?

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
| `move` | `anchor` | Re-pins the thread: re-anchoring starts from this anchor instead of the thread's own (§2.4 rule 5). The carried anchor SHOULD be an empty diff (`base == head`) at the commit the mover consulted — a statement of where the code is, not of a change |
| `mirror` | `of`, `origin` | Foreign-identity record (§8.2): the event named by `of` exists on a foreign system as `origin`. Bookkeeping, not discussion — it carries no folded state and clients SHOULD NOT render it as a message |

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
- `body`: UTF-8 Markdown (CommonMark), stored as authored — writers SHOULD NOT re-wrap or reformat it (display wrapping is a renderer concern, and forges render markdown line breaks literally, so stored wrapping would leak into exports, §8). Always inline — never a blob reference (§5.3).

### 2.3 Identifiers

- **Event ID** = lowercase hex SHA-256 of the event's canonical JSON serialization (§6), truncated to 40 characters. The event's filename is its ID. Content addressing makes merges idempotent: the same event always lands at the same path. Readers MUST verify that a stored event's bytes hash to its filename and treat files that don't as malformed (§10) — trusting the name alone would let a same-path overwrite put new content under an existing event ID.
- **Thread ID** = the event ID of its root `comment`.
- Event and thread IDs are defined by this format alone — never by the storage backend's hashing. Object IDs appearing in anchors (`diff`, `blob`) are opaque hex strings scoped to the repository's storage backend; a backend hash migration (e.g. git SHA-1 → SHA-256) changes those, but can never invalidate event IDs or the references between events.

### 2.4 State fold (normative)

Given a thread's event set:

1. **Body** of each comment/reply = the latest `edit` in its supersede-chain, ordered by (`ts`, event ID as tie-break). A `delete` tombstone wins over edits regardless of order and marks the event retracted.
2. **Resolved state** = the `resolved` value of the latest `resolve` event by (`ts`, event ID). Default: unresolved.
3. **Display order** of a thread = events ordered by (`ts`, event ID), rendered flat.
4. Events whose `in_reply_to`/`supersedes` target is absent are still valid (the target may arrive later); render them at the thread level.
5. **Effective anchor** = the `anchor` carried by the latest `move` event, ordered by (`ts`, event ID); default the thread's own `anchor.json`. Re-anchoring (§4) starts from the effective anchor. The thread's `anchor.json` stays immutable — it records what was discussed; a `move` records where that code lives now.

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
- `diff.base` / `diff.head`: the two commits whose diff the comment was made against. For a single-commit thread, `base` is the (chosen) parent of `head` — this disambiguates merge commits. For a branch-level discussion, `base` is the merge-base **at the time of commenting**, pinning exactly what the author saw. `base == head` is the **empty diff**: the thread annotates the state of `head` itself (an audit note, archaeology) rather than a change.
- `path`: the file path on `side`. `old_path` MUST be present iff the file was renamed within this diff.
- `side`: `old` (base version — e.g. deleted lines) or `new`. Comments on unchanged context lines use `new`.
- `lines`: 1-based, inclusive, **file coordinates on `side`** — never diff/patch positions.
- `blob`: SHA of the file version on `side`. Redundant by construction; serves as an integrity check. If resolving `head:path` yields a different blob, readers MUST flag the anchor instead of rendering it.
- `cols` (optional, reserved): `{ "start": n, "end": n }` sub-line span; clients MAY ignore.

`file` and `range` anchors SHOULD lie within their diff: the file changed between `base` and `head`, and `lines` overlapping a hunk on `side` or its display context (±3 lines). A comment about code irrespective of any change belongs on an empty diff, not on whichever commit was checked out.

Anchors are always valid against their own `diff` — they never "break." Displaying a thread on any *other* commit is re-anchoring (§4), which is computed, never stored.

## 4. Re-anchoring (normative)

Purpose: given an anchor `A` and a target commit `T` (e.g. current branch tip), decide where — if anywhere — to display the thread. For a thread, `A` is its **effective anchor** (§2.4 rule 5): a `move` event replaces the starting point of the algorithm, it never changes the algorithm itself.

### 4.1 The snippet (derived, never stored)

`snippet(A)` = from blob `A.blob`: the `lines` range (**target**), plus 3 lines of context **before** and **after** (fewer at file boundaries). For ranges longer than 20 lines, target = first 10 + last 10 lines, plus SHA-256 of the full range. Exporters materialize snippets at the boundary for consumers without git object access (§8).

### 4.2 The algorithm

Evaluate in order; stop at the first success. At every step, a match MUST be **unique** across all candidate files — two matches, whether in one file or split across files, mean failure of that step, never pick-first.

1. **Blob identity.** If `A.blob` exists in `T`'s tree at `A.path` (or at a rename-detected path, per `git diff -M` between `A.diff.head` and `T`), map lines 1:1. Status: `exact`.
2. **Exact snippet match.** Search the candidate files for `before + target + after` verbatim. Status: `relocated`.
3. **Fuzzed match.** Retry with fuzz level *f* = 1, 2, 3: drop *f* outer context lines from each of `before`/`after` (semantics of `git apply` fuzz). Then retry levels 0–3 with trailing-whitespace-insensitive line comparison. Status: `fuzzy(f)`.
4. **Outdated.** No unique match. Render the thread against its canonical `A.diff.head` using the derived snippet. Status: `outdated`.

Candidate files in v1 are limited to `A.path` plus rename-detected paths. Cross-file search is an explicitly non-normative client extension (§ideas).

The algorithm as written applies to `range` anchors. `commit` anchors are never re-anchored — they describe a whole change, which exists only in its own diff. `file` anchors use presence, not content: identical blob at a candidate path → `exact`; a candidate path present with different content → `relocated`; otherwise `outdated`.

Ambiguity is monotone under *relaxation*: raising the fuzz level within a comparison mode only drops constraint lines, and the whitespace-insensitive mode relaxes the byte-exact one at the same fuzz level — so ambiguity at byte-exact fuzz *f* implies ambiguity at byte-exact levels above *f* and at whitespace-insensitive levels at or above *f*, and whitespace-insensitive ambiguity implies ambiguity at every later level. The two axes are not totally ordered, though: byte-exact ambiguity at fuzz 3 (a duplicated bare target) says nothing about whitespace-insensitive fuzz 0, whose fuller context can still hold a unique match. The search therefore skips exactly the relaxations of an ambiguous level and nothing more: byte-exact ambiguity at fuzz *f* jumps to the whitespace-insensitive levels below *f*; whitespace-insensitive ambiguity ends the search. (Equivalently: evaluate all eight levels in order, treating an ambiguous level as a failed one — the skips are pure optimization.) Separately, a truncated snippet's middle is unconstrained by line matching, so a byte-exact match MUST be confirmed against the stored range hash; the whitespace-insensitive pass cannot be, which is one reason its results are `fuzzy(f)`, never `relocated`.

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
- **Anchored-commit retention:** for each distinct `diff.head` referenced by newly added anchors — thread anchors and anchors carried by `move` events alike — the publish commit MUST list that commit as an **additional parent**. Reachability from `refs/threads/data` then keeps discussed commits alive in every clone that fetches threads — surviving branch deletion, squash merges, and server GC. (`diff.base` needs no parent: it is an ancestor of `head` in both the commit-diff and merge-base cases.)
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
fetch = +refs/threads/data*:refs/threads/remotes/<remote>/data*
```

Fetch refspecs are additive, so this augments normal fetches. The glob form is required: git treats a configured *exact* refspec as mandatory and fails the whole fetch when the ref is missing, so the exact form would break plain `git fetch` until the remote has threads data. A glob that matches nothing is silently skipped. Two deliberate asymmetries:

- **Remote state lands in a tracking ref, never directly on `refs/threads/data`.** A direct `+refs/threads/*:refs/threads/*` mapping would let any fetch force-overwrite the local ref, making unpublished local events unreachable. Integration into the local ref is a separate step under the tool's control (§7.2): fast-forward when possible, union merge otherwise — exactly git's own branch/tracking-ref model. Unlike a branch merge, it can never conflict and never discards local events, so clients SHOULD run it opportunistically (e.g. before every command) — after `init`, a plain `git fetch` is all it takes for new data to appear. The tracking ref lives under the format's own namespace (not `refs/remotes/*`, which a branch named `threads/data` could collide with).
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

The forge review-thread model (root comment at a diff position, `in_reply_to` replies, resolved bit) maps ~1:1 onto this format, and anchors already store what forge APIs speak: file coordinates on a side (§3), never diff positions. Consumers without git object access (static HTML, email) get context by materializing derived snippets (§4.1) at the export boundary.

### 8.1 Import

Foreign comments become threads with anchors reconstructed from the forge's position data; foreign identity maps into `author`. Each imported event MUST carry its foreign ID in an `origin` field — the key that makes re-imports no-ops:

```json
"origin": { "forge": "github", "id": "<comment-id>", "url": "https://..." }
```

An importer MUST skip foreign items whose ID the store already records — in an imported event's own `origin`, or in a `mirror` event (§8.2), in which case references to the foreign item resolve to the event its `of` names. Imports SHOULD be deterministic — every event's bytes a function of forge data and the git DAG, never of import time — so independent clones importing the same discussion mint identical event IDs and the union merge (§7.2) dedupes them.

### 8.2 Export and the `mirror` event

Export posts threads to a foreign change (PR/MR): every thread in the change's range, and within each, every folded event (§2.4) that has no foreign identity yet — no `origin` of its own and no `mirror` naming it. A thread imported from that same change thus contributes only what was said locally since import, posted into the existing foreign thread via the imported root's `origin`.

Events are append-only and content-addressed, so a published event can never be stamped with the identity the forge mints for it at post time. Recording that identity is the `mirror` event's job, appended to the thread after each successful post:

```json
{
  "v": 1,
  "type": "mirror",
  "author": { "name": "...", "email": "..." },
  "ts": "2026-07-31T09:15:02Z",
  "of": "<event-id>",
  "origin": { "forge": "gitlab", "id": "<note-id>", "url": "https://..." }
}
```

`of` names the exported event; `origin` is its foreign identity, same shape as on import. `author` is the account that posted; `ts` SHOULD be the creation time the forge reports, not the exporter's clock. Because mirrors are shared data, one clone's export makes every clone's re-export a no-op — and inoculates every clone's import against re-importing the posted comments as foreign ones.

Positions: re-anchor (§4) the effective anchor to the change's head; a unique match visible in the displayed diff exports as a line comment. Otherwise degrade honestly: a file-level comment when the path is in the diff, else a change-level comment carrying the materialized snippet. `commit` anchors are always change-level.

Resolution is a single bit on a foreign thread, not an event log, so per-event tracking cannot express reopen cycles: exporters MUST reconcile it by comparing folded state against foreign state and toggling on mismatch. The mirror recorded for a toggle carries the foreign *thread* ID — the same ID import's synthetic resolve dedups on.

Everything posts under the exporting account; when an event's `author` is someone else, exporters SHOULD prepend an attribution header (author, timestamp) to the posted body.

Export is idempotent per store but not atomic across stores: forges offer no compare-and-swap, so two clones exporting the same new events concurrently can double-post. Exporters MUST NOT export draft events and SHOULD integrate remote thread data (§7.2) before posting.

### 8.3 Round-trip

Import and export compose into poll-based bidirectional mirroring with no state beyond `origin` and `mirror`: each direction skips what the other recorded, replies land in the right thread on both sides (import wires `in_reply_to` through mirrored IDs; export posts into the foreign thread the imported root names), and resolution reconciles by state comparison. Live sync (webhooks, notifications) remains delegated to the transport (§1).

## 9. Scaling notes (informative)

Back-of-envelope for a 500k-line repo, 50 contributors, ~5,000 reviews/year (~125k events):

- ~25–30 MB/year packed (blobs + commits + tree deltas) — same order as the code history itself. Five years ≈ 150 MB.
- Ref contention: batched publishes ≈ 8/hour peak; the retry loop absorbs 10× that without coordination.
- Reads ("which threads touch this file?") require an index — deliberately **not** part of the format (derived data). Clients build a local index (e.g. SQLite: path → thread IDs) in one full scan, then update incrementally from tree diffs per fetch.
- Loose-object accumulation is handled by git auto-gc at these volumes; a `maintenance` command is optional hygiene.
- Honest ceiling: monorepo scale (thousands of contributors) would need sharded/per-period refs — the layout permits it, v1 does not define it.

## 10. Versioning

- Every document carries `"v"`. Readers MUST accept documents with unknown fields and SHOULD surface (not crash on) unknown major versions.
- Additive changes (new optional fields, new event types) do not bump `v`. Semantic changes to the fold, the algorithm, or canonical serialization do.
- **Malformed data never poisons the read.** The data ref is shared and append-only, so a reader that hard-fails on one bad file — a buggy or hostile writer's — is broken in every clone, forever. Readers MUST skip, with a diagnostic, files that fail to parse, event files whose bytes don't hash to their filename (§2.3), and thread directories with no readable anchor — and render everything else. Writers MUST still validate everything they write.

---

## Ideas for later

Format & semantics

- **Suggested changes**: patch-carrying events (GitHub "suggestion" blocks) applied with `git apply`; attachments already give them a reachable home.
- **Review verdicts**: approve / request-changes events; per-review summary rollup.
- **Patchset tracking** (Gerrit-style): link successive heads of a rebased series so threads follow a PR across force-pushes. Largely derivable with no stored state: `git patch-id --stable` is identical across a rebase, and §5.2 retention keeps the pre-rewrite commit readable in every clone, so a client can find an anchored commit's rewritten twin without having witnessed the rewrite — which is the normal case, since the rewriter is rarely the reader. What patch-id cannot recover is where the diff genuinely changed: conflict resolution, reordering commits that touch the same lines, and squash-merges of more than one commit (a squash reclassifies lines an earlier commit added as additions again, so no constituent patch-id survives). That residue is what `move` (§2.4 rule 5) is for.
- **Stable change identities**: an optional `change_id` anchor field for backends with rebase-stable identities (jj change IDs, Gerrit Change-Ids), letting a thread follow a logical change across history rewrites — complementing, and possibly subsuming, patchset tracking.
- **Reactions** (👍 events), **labels/tags** on threads.
- **Sub-line anchors**: activate the reserved `cols` field (agents want exact spans).
- **Cross-file re-anchoring**: content search beyond the anchored path (survives file splits); high false-positive risk, spec as an interactive client extension with distinct status.
- **Signing**: `sig` on events (SSH/GPG, same trust model as commit signing) for authenticity in low-trust repos.

Storage & scale

- **Sharded refs** (per-shard or per-quarter) past ~10× the v1 scaling envelope.
- **`refs/notes/threads` bridge**: derived per-commit summaries ("3 threads, 1 unresolved") so vanilla `git log --notes=threads` shows discussion presence.
- **Maintenance command**: repack, commit-graph, index rebuild.

Tooling

- **CLI** — the single-player wedge. The reference implementation covers `comment|reply|edit|delete|resolve|move|discard|show|list|status|seen|pull|commit|push|init|import` (search via `list --grep`, machine output via `--json`, GitHub import per §8); `export` remains.
- **GitLab importer and exporter** (the GitHub importer ships in the reference CLI); with export (§8.2) in place, bidirectional sync is a polling loop, not new format.
- **Re-pinning orphans in bulk**: after a squash-merge or force-push, threads pinned to commits the rewrite left behind (still readable — §5.2 — just no longer part of the branch under review) can be located by re-anchoring (§4) and re-pinned with `move` events — a client command, no format change, and the result is shared data, so one person's run fixes the view for everyone. A git hook is the wrong shape here: forges squash server-side, where nothing local fires. Which threads are candidates is decided by re-anchor status, never by reconstructing what happened to the commit: a unique match (`exact`/`relocated`) at the target means the discussed code is there and the thread belongs there; no match means it isn't — whether the commit was dropped, reverted, or simply not merged yet — and the thread stays put. That test is also the orphan definition: off-target alone means nothing (threads on in-flight branches and unmerged imports legitimately live elsewhere); off-target *with* a unique match at the target is what a re-pin command reports and acts on. Content is the right question, not history: a dropped commit whose code someone else reintroduced *should* re-pin, and a squashed commit whose lines were reverted later in the same branch should not. `git range-diff` can pair old↔new commits beyond patch-id's exact twins, but in testing only at a tuned `--creation-factor` — a knob two clients would disagree on — so it is at most an interactive aid, never membership. `commit`-kind anchors are never re-anchored (§4.2), so a patch-id twin is their only automatic rescue.
- **Static HTML export** of a discussion for repo-less readers.
- **Desktop review client**: syntax highlighting, LSP navigation, search — the niceties web review UIs lack.
- **Agent integration**: discuss changes with an agent whose commentary persists as threads; conventions for agent long-form output (attachments, soft body-size cap — agent verbosity is the assumption most likely to break the storage math).
- **Notification bridge**: polling fetch, webhook adapter, or email digest — how a colleague learns there's something new.
- **Client-local niceties** (explicitly outside the shared format): draft/unpublished comments (the reference CLI stages them on a local-only `refs/threads/drafts`), read-unread tracking (the reference CLI keeps a seen snapshot on a local-only `refs/threads/seen`), re-anchor cache (the reference CLI keeps one under `.git/threads/reanchor/`).

Open questions

- Second-precision timestamps make same-second updates order-undefined beyond the deterministic ID tie-break (e.g. a resolve→reopen toggle within one second, or a reply displaying before its root). Convergence is unaffected; causal intuition is. Millisecond precision, or a per-writer logical counter?
- Threading model for `edit` by non-authors (moderation?).
- Anchors into submodules.
- jj colocated repos are expected to work unmodified (the git backend stores real refs and objects) — verify, and check that anchored-commit retention holds under jj's own GC of hidden commits.
- Very large monorepo path indexing.
- Privacy: thread data is world-readable to anyone with repo access; is per-thread encryption ever worth the complexity?

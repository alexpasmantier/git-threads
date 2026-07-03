# Deep dive: synchronization

How discussion data moves between clones without conflicts and without a coordinating server.
Spec: [SPEC.md](../SPEC.md) §7. Code:
[`crates/git-threads/src/commands.rs`](../crates/git-threads/src/commands.rs) (`init`, `pull`,
`commit`, `push`, `fetch_and_integrate`) and
[`crates/git-threads/src/store.rs`](../crates/git-threads/src/store.rs) (`integrate`,
`union_trees`).

## Configuration: one refspec, and why exactly that one

`git threads init` adds a single fetch refspec to the remote:

```
fetch = +refs/threads/data:refs/threads/remotes/<remote>/data
```

Every design constraint here was learned the hard way (one of them by this very repository —
see below):

- **Remote state lands in a tracking ref, never directly on `refs/threads/data`.** The naive
  mapping `+refs/threads/*:refs/threads/*` means every plain `git fetch` force-overwrites your
  local ref with the remote's — and if you had unpublished local events, the only pointer to
  them is gone. This exact bug shipped in this repo's first `init` implementation and was
  found while dogfooding; the discussion thread that reported it, anchored to the offending
  line, lives in the repo's own threads data (thread `84727c6d`). The fix mirrors git's own
  branch model: fetch updates `refs/threads/remotes/origin/data` (analogous to `origin/main`),
  and moving *your* ref is a separate, explicit integration step.
- **The tracking ref lives under `refs/threads/remotes/…`, not `refs/remotes/…`** — a branch
  literally named `threads/data` would collide with the standard namespace.
- **No push refspec is configured.** Setting `remote.<name>.push` *replaces* git's default
  push behavior for the whole clone — a bare `git push` would silently stop pushing the
  current branch. Publishing pushes explicitly instead.
- **Self-healing:** `pull` and `push` pass the full refspec explicitly on every fetch, so
  they work (and repair themselves) even in a clone that never ran `init`. Git config and
  hooks are per-clone by design; "install the tool, run any command once" is the setup floor.

## The publish loop

Two commands share the work, carrying their exact git meanings: `git threads commit` seals
all drafts into the local data ref as one batch (no network — see the
[drafts staging area](storage.md#writes)), and `git threads push` runs the loop the spec
calls publishing:

```
0. COMMIT     drafts ref  →  one commit on local refs/threads/data   (git threads commit)
   ── everything below is `git threads push` ──
1. FETCH      remote refs/threads/data  →  tracking ref
2. INTEGRATE  tracking ref  →  local refs/threads/data
3. PUSH       local refs/threads/data  →  remote (explicit refspec)
   rejected because someone else pushed first?  →  go to 1
```

`push` never touches drafts — nothing leaves your machine without an explicit `commit` — and
prints a reminder when drafts are sitting uncommitted.

Integration (step 2) has four cases, checked in order:

| remote tip is… | action | outcome |
|---|---|---|
| absent | nothing to integrate | — |
| already contained in local history | no-op | `up to date` |
| a descendant of local (or local absent) | move the ref forward | `fast-forwarded` / `initialized` |
| diverged | **tree union merge**, both tips as parents | `merged` |

Every local ref update is compare-and-swap on the expected tip, so a concurrent local writer
causes a clean retryable failure, never a lost update.

The push (step 3) can only be rejected for non-fast-forward — meaning a concurrent publisher
won the race and the remote has events we haven't integrated yet. The loop re-fetches,
re-unions, re-pushes. Each retry strictly grows the local event set, so the loop converges;
the CLI caps it at 5 attempts as a safety valve (the spec's envelope estimates peak publish
rates where even 10× contention is absorbed by a couple of retries).

## The union merge

The heart of conflict-freedom. When histories diverge, the merge commit's tree is computed by
recursively unioning the two tip trees:

- an entry present on one side only → take it
- present on both sides with identical content → take it
- both sides are subtrees → recurse

There is no conflict case *by construction*: writers only ever add files whose names are
content hashes, so two histories can never hold different content at the same path. What looks
like a semantic conflict — you resolved a thread while I reopened it — is just two event files
with different names, both kept; the [fold](state-fold.md) picks the winner at read time. Merge
time is the wrong place to resolve anything, and this design never has to.

One paranoid corner: if malformed data *does* put different content at the same path (a
hand-edited event file, a buggy tool), the union picks deterministically (trees over blobs,
then larger object ID) rather than erroring — because all replicas picking the *same* winner
keeps the network convergent, while an error would wedge every publish that touches the path.

Append-only discipline follows from all this: history on the threads ref is never rewritten
and the ref is never force-pushed. Rewriting would orphan events that other clones' merges
still reference — the one thing the design cannot repair automatically.

## Hosting

Plain git servers, GitHub, and GitLab all accept pushes to `refs/threads/*` — it's an ordinary
ref namespace; they store it and simply don't render it. Branch protection rules don't
normally cover it. Two consequences worth knowing:

- Default clones and forks don't copy the namespace; threads arrive on `init`'s first fetch.
- Access control is whatever the transport gives you: anyone who can push branches can push
  threads. Identity is unverified until event signing exists (reserved `sig` field).

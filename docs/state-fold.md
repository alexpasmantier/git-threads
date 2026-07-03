# Deep dive: the state fold

How append-only events become the conversation you see. Spec: [SPEC.md](../SPEC.md) §2.4.
Code: [`crates/git-threads-core/src/fold.rs`](../crates/git-threads-core/src/fold.rs).

## The contract

The fold is a pure function from a thread's event **set** to its display state:

```
fold : { (event-id, event) } → { resolved: bool, messages: [ (text, edited, retracted, order) ] }
```

Two properties are non-negotiable, and everything below is in their service:

1. **Determinism across replicas.** Everyone holding the same events sees the same
   conversation. This is what lets [sync](sync.md) be a dumb file union: agreement is
   reconstructed at read time, not negotiated at merge time.
2. **Input-order invariance.** Events arrive in arbitrary order (merges, offline writers,
   filesystem listing order). The fold treats its input as a set; a property test feeds it
   rotations and reversals of the same events and requires identical output.

## The rules

### Ordering and ties

Display order is `(ts, event-id)` — timestamp first, content-hash ID as the deterministic
tie-break. The tie-break guarantees *convergence*, not *causality*: two events in the same
second order arbitrarily-but-identically on every machine. That's the format's standing open
question (millisecond precision? per-writer counters?), and why the CLI plays it safe — see
[edit chains](#edit-chains) below.

### Resolution

`resolved` = the value carried by the **latest** `resolve` event by `(ts, id)`; default
unresolved. Concurrent resolve/reopen races are settled here, at fold time — the storage layer
happily keeps both events.

### Edit chains

An `edit` names the event it replaces in `supersedes`. Chains can be deep (edit of an edit).
For each displayed message the fold:

1. collects edits targeting it, **keeping only those whose author email matches the message's
   author** — the spec restricts editing to the author; foreign "edits" are stored (append-only
   means nothing is rejected at the storage layer) but never take effect,
2. picks the winner by `(ts, id)`, moves to it, and repeats from there — following the chain
   through winners until it ends,
3. guards against cycles in malformed data with a visited set.

The message's effective text is the final winner's body; `edited` is flagged. The fold also
reports the chain's tip (`FoldedEvent::chain_tip`) so a *new* edit can supersede the tip
rather than the root. That keeps chains linear: two edits both superseding the root would be
same-second siblings whose winner depends only on the ID tie-break — convergent, but
arbitrary. The CLI always chains from the tip for this reason.

### Tombstones

A `delete` retracts a message if it targets **any node of the message's winning edit chain**
(same-author rule applies). Tombstones win *regardless of timestamp* — a delete followed by a
later edit still leaves the message retracted. Rationale: retraction is the one operation
where "latest wins" would be wrong; someone who deleted a message meant it, and an edit racing
with a delete shouldn't resurrect content. The body stays in the stored event (history is
never rewritten); the fold just reports `retracted` and renderers hide the text.

### Orphans

A `reply` or `edit` whose target isn't in the set is still valid — its target may simply not
have arrived yet (partial sync). Orphaned replies render at thread level; orphaned edits sit
idle until their target appears. Nothing is ever dropped for dangling references.

### Duplicates

Input is deduplicated by ID before folding. Content addressing makes duplicates
byte-identical, so which copy survives is immaterial.

## What the fold deliberately does not do

- **Moderation.** Non-author edits and deletes have no effect. Whether some identity should be
  able to moderate others' messages is an explicit open question — and note that author
  identity is git-style (unverified strings), so any enforcement here is convention until
  event signing lands.
- **Validation.** The fold folds what it's given, including events of unknown type (ignored
  for display, preserved in the set). Schema validation happens at write time.
- **Threading.** Replies carry `in_reply_to`, but rendering is flat by spec (v1). The field is
  preserved for future tree rendering and for exporters that need it (GitHub's model, e.g.).

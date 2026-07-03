# Documentation

Start here:

- **[How it works](how-it-works.md)** — the guided tour in plain language, with diagrams.
  No git internals required.

Deep dives, one per subsystem — design rationale, exact mechanics, edge cases, and pointers
into the spec and code:

- **[The data format](format.md)** — events, anchors, canonical JSON, content-addressed IDs,
  forward compatibility.
- **[The state fold](state-fold.md)** — how append-only events deterministically become
  conversation state: edit chains, tombstones, ties, orphans.
- **[Storage](storage.md)** — the snapshot tree on `refs/threads/data`, batched writes,
  retention parents, why bodies are inline, scaling.
- **[Synchronization](sync.md)** — the refspec design, the publish loop, the conflict-free
  union merge, hosting behavior.
- **[Re-anchoring](reanchoring.md)** — snippets, the four-rung ladder, fuzz semantics,
  ambiguity rules, and its behavior on this repo's own history.

The normative rules live in [SPEC.md](../SPEC.md); these documents explain and motivate them.
Where implementation experience changed the spec (it has, several times), the deep dives tell
that story.

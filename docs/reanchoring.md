# Deep dive: re-anchoring

How a thread pinned to code that has since moved, changed, or vanished finds its place in the
version you're looking at. Spec: [SPEC.md](../SPEC.md) §4. Code: the pure matching in
[`crates/git-threads-core/src/reanchor.rs`](../crates/git-threads-core/src/reanchor.rs) and
[`snippet.rs`](../crates/git-threads-core/src/snippet.rs); the git-facing steps in
[`crates/git-threads/src/reanchor.rs`](../crates/git-threads/src/reanchor.rs).

## Framing: anchors are facts, positions are views

An anchor immutably records what the commenter saw (diff, path, side, lines, exact blob).
It is always valid against its own diff and can never break. "Where should this thread appear
on commit `T`?" is a separate question, answered by a **pure function of `(anchor, T)`** —
computed on demand, identical on every machine, cacheable forever, and never stored in the
shared format. This split is what lets the stored data stay immutable while the display stays
current.

## The snippet: the search needle

Derived (never stored) from the anchor's own blob:

- the anchored lines (the **target**), plus up to 3 lines of context on each side — fewer at
  file boundaries;
- ranges longer than 20 lines don't carry every line: the needle becomes the first 10 + last
  10 lines, the count of omitted middle lines, and a SHA-256 of the full range.

The parameters (3 / 20 / 10+10) are normative, not tunable — two implementations must derive
the same snippet or they'd disagree about matches. Exporters also materialize snippets at the
boundary, so consumers without git object access (static HTML, email) can render context.

## The algorithm

Evaluated strictly in order; first success wins. At every step a match must be **unique across
all candidate files** — two positions, in one file or split across files, fail that step,
never pick-first.

### Step 1 — blob identity → `exact`

If the anchored blob itself (same content hash) sits in `T`'s tree at the anchored path — or
at a rename-detected path — the file version the comment was made on is present verbatim.
Lines map 1:1; nothing to search. This is a tree lookup, no content comparison at all, and it
covers the overwhelmingly common case (the file didn't change, or only *other* files did).

Candidate paths throughout the algorithm are the anchored path plus rename-detected successors
(`git diff -M` between the anchor's `head` and `T`). Rename detection is best-effort: if the
diff can't run, the candidate list just shrinks to the anchored path. Cross-file content
search is explicitly out of scope for v1 (high false-positive risk; a possible interactive
client extension).

### Step 2 — verbatim snippet → `relocated`

Search each candidate file for `before + target + after`, byte-exact. A unique hit means the
code moved but didn't change (lines inserted above, file reshuffled). New line numbers are
reported; the position of the target inside the matched pattern gives the mapping.

### Step 3 — fuzzed match → `fuzzy(f)`

Two relaxations, tried in order:

1. **Context fuzz** `f = 1, 2, 3` (the semantics of `git apply`'s fuzz): drop the `f`
   *outermost* context lines from each side of the needle and search again. At `f = 3` the
   needle is the bare target. The target lines themselves are never fuzzed.
2. **Trailing-whitespace-insensitive retry** of levels 0–3: line comparison ignores trailing
   whitespace (a formatter pass shouldn't orphan every thread).

Any success here is `fuzzy(f)` — including whitespace-insensitive level 0, which is
deliberately *not* `relocated`: `relocated` promises verbatim.

### Step 4 — `outdated`

No unique match anywhere: the discussed code is genuinely gone or changed beyond recognition.
The thread renders against its own `diff.head` using the derived snippet — always possible,
because [retention parents](storage.md#retention-parents-discussed-commits-cant-be-garbage-collected)
guarantee the original objects exist. An outdated thread is never lost; it's labeled history.

It doesn't have to stay that way: `git threads move` re-pins the thread by hand (a `move`
event carrying a fresh anchor, SPEC.md §2.4 rule 5), and the algorithm starts from the new
anchor on every later lookup. Human judgment supplies what content matching couldn't —
across file splits, rewrites, renames-with-changes — and the moved thread ages gracefully
again, because the new anchor re-anchors like any other.

### Kind-specific rules

The algorithm as described applies to `range` anchors. `commit` anchors are never re-anchored —
they describe a whole change, which exists only in its own diff. `file` anchors use presence,
not content: identical blob → `exact`; candidate path present with different content →
`relocated`; otherwise `outdated`.

## Implementation notes

**Ambiguity short-circuits — but only along relaxations.** Raising the fuzz level within a
comparison mode only ever *relaxes* the needle (drops constraint lines), and the
whitespace-insensitive mode relaxes byte-exact at the same fuzz level — every position
matching before still matches after, so ambiguity survives every relaxation. (Also why
ambiguity can't be "resolved" by fuzzing harder.) The two axes are *not* totally ordered,
though: a bare target duplicated in the file makes byte-exact fuzz 3 ambiguous, yet
whitespace-insensitive fuzz 0 — full context, compared loosely — can still hold a unique
match. That's the formatter case: trailing whitespace changed on every context line, target
untouched. So the search skips exactly the relaxations of an ambiguous level and nothing
more: byte-exact ambiguity at fuzz `f` jumps to the whitespace-insensitive levels below `f`,
and whitespace-insensitive ambiguity ends the search.

**Truncated needles verify the hash.** A >20-line target's middle is unconstrained by line
matching — the needle only pins head, tail, and the gap length. In byte-exact mode the stored
SHA-256 of the full range must confirm the candidate's middle; a middle that changed fails
step 2 honestly. The whitespace-insensitive pass *cannot* verify the hash (trimmed lines hash
differently), which is the second reason its results are capped at `fuzzy`.

**The pure/git split.** Steps 2–3 are pure string matching (`locate_snippet(snippet, content)`
in the core crate — no I/O, no git, property-tested in isolation, WASM-friendly). Step 1,
candidate discovery, and blob loading are the CLI crate's thin git-facing wrapper. The spec's
determinism requirement falls out of the purity.

**Caching is sanctioned but local.** `(anchor, T)` are both immutable, so results are
cacheable forever. Caches are client-local derived data, never part of the shared format —
same policy as search indexes. The reference CLI keeps one under
`.git/threads/reanchor/<target>.json` (one file per target commit, oldest aged out), which
is what makes `list` fast on repositories with hundreds of threads; deleting the directory
is always safe. Rename detection is also skipped whenever the anchored path still exists on
the target — a surviving path can never be a rename source, so most threads never pay for
it.

## Observed behavior in this repository

The repo's own threads exercise every step on real history:

- **`exact`, backwards in time**: the thread on the old fetch-refspec bug, re-anchored *onto
  its original commit* (`show <id> --at <old-head>`) — a commit that survives only through
  retention parents — matches by blob identity at its original line.
- **`fuzzy(3)`, live**: the SIGPIPE thread anchored to `fn main() {` has followed that line
  across several refactors (94 → 104 → 111) as code was inserted around it — its immediate
  context changed (the fix itself sits in the after-context), so only the bare-target level
  matches, and the status says so.
- **`outdated`, honestly**: that same fetch-refspec thread against current `HEAD` — the line
  it discussed was deleted by the fix it prompted. Its `show` renders the original context.

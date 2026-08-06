# Worktree-aware display

Status: piece 1 (display) implemented 2026-08; piece 2 (creation) deliberately held until
dogfooding asks for it.

## The quirk

The local review loop — read code, draft comments, then edit the code — breaks down in the
window between editing and committing. Everything in git-threads speaks in committed
coordinates: `comment` validates lines against HEAD's tree, and placement is a pure function
of (anchor, target *commit*), cached per commit. The working tree is never consulted. So the
moment a reviewed file is edited on disk:

- `list`/`show` keep reporting HEAD-relative locations ("`code.txt:10-12 (exact)`") while the
  editor shows that code at 20-22. Honest against HEAD, a lie against the screen.
- `show`'s excerpt renders HEAD's text for lines rewritten on disk.
- Creating a comment with the line numbers the editor shows misfires: out-of-range or
  outside-the-diff errors — or, silently worse, valid-in-HEAD numbers anchoring the wrong code.

Committing heals everything (the §4.2 ladder re-anchors). The gap is exactly the uncommitted
window where local review happens.

## Walls (what keeps this from becoming a monster)

1. **`reanchor()` stays pure.** Placement against a commit remains a pure, cached function.
   The worktree is a small *second pass* at the display edge, applied after the cache and
   never cached itself — the tree mutates, and the pass is cheap (one file's content).
2. **The index is invisible.** Disk is the only worktree truth: the editor shows the disk,
   so locations map to the disk. No third coordinate space for staged content.
3. **No deferred anchors.** A comment on uncommitted lines is refused, on principle: threads
   are shareable data anchored in history, and nobody else has an unsaved buffer. Draft-time
   anchors that resolve at commit time are a different data model; if ever, they get their
   own design.
4. **The shared format is untouched.** Anchors, events, the spec — nothing changes. This is
   client-side presentation, SPEC.md "client-local niceties" territory.

## Piece 1: the display pass (implemented)

When re-anchoring targets the checked-out HEAD (the default) and a placed file is dirty on
disk, the placement is re-located in the disk content and tagged:

- `list --oneline`: `code.txt:20-22 (relocated, worktree)` — the tag appends to the usual
  status; an exact worktree hit shows bare `(worktree)`.
- `show`: `Current: code.txt:20-22 in the working tree (relocated)`; when the commented code
  is not findable in the dirty file, `no match in the working tree`.
- `--json`: the thread object gains `"worktree": true` (absent otherwise); the placement
  object's shape is unchanged.

Mechanics: same ladder, one candidate. Dirty = the disk file's *lines* differ from the
placed blob's (line-based, so checkout CRLF filters don't fake dirtiness). Then blob
identity against the anchored blob, then the anchor's derived snippet located in the disk
content (`locate_snippet`), else no match. `show`'s file excerpt reads the disk when the
placement does, so the excerpt agrees with the `Current:` line. Clean tree → byte-identical
behavior to before.

Not remapped, deliberately: placements already `outdated` at HEAD (the anchored code was
gone before any local edit), file-kind anchors (the path is the location), and explicit
`--at <commit>` targets (pinned means pinned).

## Piece 2: creation mapping (held)

Interpret `comment path:lines` against the disk file when dirty, then map the lines back
through the disk-vs-HEAD diff to store the anchor in HEAD coordinates — the hunk arithmetic
already exists (`old_line_of`/`hunk_pairs` in the GitLab adapter, pointed at a different
diff). Lines that are themselves uncommitted get a refusal that says why ("line 21 is
uncommitted; commit first"). Held because the reported pain was comment-first-then-edit,
where creation works and only display lied; real friction decides whether this ships.

## Rejected

- **Worktree as a first-class re-anchor target** (polymorphic `reanchor()`): poisons the
  purity/caching property everywhere for one display concern.
- **Anchoring drafts to odb-written worktree blobs** (`git hash-object -w`): drafts become
  events verbatim at `commit`; shared anchors would reference blobs unreachable from any
  commit — breaks the shareable-data invariant.
- **Opt-in flag (`--at WORKTREE`) instead of default**: a default that lies whenever the
  tree is dirty, with the truth behind a flag, keeps the exact quirk this exists to fix.
  The clean-tree case is unchanged, so the default flip has no blast radius beyond the
  dirty window.

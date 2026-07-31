# Exporting threads to a PR/MR

Status: design agreed 2026-07-31, not implemented. Work is tracked in the plan at the bottom.

## Goal

`git threads export <forge> <pr|mr>` posts local thread events onto a forge review, the mirror
image of `import`. The end goal is bidirectional sync: alternating `import` and `export` runs
converge both sides, with no state beyond what each direction records in the store. GitLab is a
first-class target alongside GitHub — one forge-neutral planner, thin per-forge executors.

The format side — the `mirror` event, dedup rules, position/resolution/attribution semantics —
is SPEC.md §8. This doc records the decisions and the implementation plan.

## Decisions

- Export selects **all threads in the change's range, open and resolved**. The motivating loop:
  a reviewer's MR thread is imported, answered and resolved locally, and the re-export must show
  the answer and the resolution on the forge.
- Export posts **events, not threads**: any folded event without a foreign identity (its own
  `origin`, or a `mirror` naming it). A thread imported from the same MR contributes only the
  replies/resolution added locally since import, posted into the existing forge discussion via
  the imported root's `origin`.
- The exported-ness record is the **`mirror` event** (SPEC §8.2) — shared data, so one clone's
  export makes every clone's re-export a no-op, and import can't boomerang exported comments
  back as foreign ones. Client-local state can't give either guarantee.
- **Resolution syncs by state comparison** (folded state vs the forge's resolved bit), never by
  per-event markers — the only scheme that survives reopen cycles. The toggle's mirror carries
  the foreign thread ID, which is exactly what import's synthetic-resolve dedup keys on.
- **Attribution header only on author mismatch.** Everything posts under the token's account;
  when the event author differs, the body gets a header line (`**name** · date · via
  git-threads`). One API call tells the executor who the token is.

## Selection

Threads whose effective anchor lies in the PR's range — the `list <base>...<head>` machinery,
patch-id twins included — plus threads imported from this same PR (`origin` match, as in
`list --pr`), which may have drifted out of the range after a force-push but still belong to the
conversation.

Threads imported from a *different* PR or forge are skipped with a warning in v1: their events
already have foreign identities elsewhere, and posting half a thread is worse than linking to it.

## Positions

Decided offline by the planner, per thread:

1. Re-anchor the effective anchor to the PR head. A unique match (`exact`/`relocated`/`fuzzy`)
   landing inside the displayed diff (hunks ± context — what `hunk_spans` computes for
   `comment` placement today) → line comment: `side`/`line`/`start_line` in file coordinates,
   which is what both forges' current APIs take.
2. Path in the diff but the line isn't → file-level comment (GitHub `subject_type: file`;
   GitLab: positionless discussion naming the file).
3. Otherwise — and for `commit` anchors and `outdated` re-anchors — a change-level comment with
   the derived snippet (SPEC §4.1) materialized in the body plus a `blob/<head>/<path>#L<n>`
   permalink. GitLab's positionless discussions are still resolvable threads; GitHub issue
   comments are not, so resolution is skipped there with a note.

Old-side anchors (comments on deleted lines) translate only when the anchor's `base` equals the
PR's merge-base; otherwise they take the same fallback.

## Architecture

Mirror of `import.rs`, in `crates/git-threads/src/export.rs`:

- **Pure planner** — `plan(threads, pr_state) -> Vec<Action>`,
  `Action = CreateThread | Reply | ToggleResolve | Fallback | Skip { reason }`. No network, no
  git writes; unit-testable like import's `apply`, and `--dry-run` is just printing the plan.
- **Executors** — GitHub via `gh api` (REST for create/reply, GraphQL for resolve — REST
  can't), GitLab via `glab api` (discussions API; position payload = `base_sha`/`start_sha`/
  `head_sha` from the MR's `diff_refs` + `new_path`/`new_line`). Same seam philosophy as
  import: auth and pagination for free, no HTTP stack in the tree.
- Posting is sequential (forge content-creation rate limits; ~1/s is safe on GitHub), replies
  after their parents, `mirror` markers written per thread as publish commits — an interrupted
  run keeps its progress and resumes idempotently.
- Drafts are never exported (a discarded draft would leave a forge comment naming an event that
  never existed). Export integrates remote thread data first.

Unlike import, export cannot be deterministic — the forge mints IDs at post time — so two clones
racing an export can double-post. No forge offers compare-and-swap; documented, mitigated by
integrate-before-export.

Touchpoints outside the new file:

- `git-threads-core`: `EventKind::Mirror`, a typed `of: Option<EventId>` field on `Event`,
  validation row (requires `of`; forbids `body`/`in_reply_to`/`supersedes`/`resolved`/`anchor`).
- Fold/render: mirror events carry no folded state by construction; `show`/`list` must not
  render them as messages.
- `import.rs` `origin_index`: when the event carrying `origin` has `of`, index the foreign ID
  against the event `of` names. This one rule closes the round-trip: import skips comments we
  posted, and a forge reply to an exported comment imports with `in_reply_to` wired to the
  right local event.
- `main.rs`: `Export { source }` mirroring `Import { source }`.

## Punted (deliberately)

- Propagating post-export edits/deletes to the forge (PATCH). The mapping makes it possible
  later; v1 is create + reply + resolve.
- Batched review creation (GraphQL `addPullRequestReview` with `threads: []` — one notification
  instead of N). Different failure semantics; revisit after v1.
- Cross-target export (e.g. forge migration). Representable — an event can accumulate mirrors
  on several forges — but out of scope.

## Plan

- [x] SPEC.md §8: `mirror` event, dedup and export rules
- [ ] core: `EventKind::Mirror` + `of` field + validation + fold/render exclusion
- [ ] `export.rs` planner + offline tests
- [ ] GitHub executor + `export github <pr|url>` CLI + the `origin_index` `of` rule
- [ ] round-trip test: export → import is a no-op; a forge reply imports onto the right event
- [ ] GitLab executor + `export gitlab <mr|url>`
- [ ] GitLab importer (prerequisite for the full GitLab loop)

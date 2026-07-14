# Deep dive: the data format

Events, anchors, canonical JSON, and content-addressed IDs — the layer everything else is
built on. Spec: [SPEC.md](../SPEC.md) §2, §3, §6, §10. Code:
[`crates/git-threads-core`](../crates/git-threads-core/src/) (`event.rs`, `anchor.rs`,
`canonical.rs`, `id.rs`).

## Events

Every event is one JSON document with a fixed envelope and per-type payload fields:

```json
{
  "v": 1,
  "type": "reply",
  "author": { "name": "Alex", "email": "alex@example.com" },
  "ts": "2026-07-03T14:12:09Z",
  "in_reply_to": "84727c6d0f7c21c40ee8768996a20f540d6b1304",
  "body": "good catch, no — fixing"
}
```

Which payload fields are required and which are forbidden is strict, per type:

| type | required | forbidden |
|---|---|---|
| `comment` | `body` | `in_reply_to`, `supersedes`, `resolved` |
| `reply` | `body`, `in_reply_to` | `supersedes`, `resolved` |
| `edit` | `body`, `supersedes` | `in_reply_to`, `resolved` |
| `resolve` | `resolved` | `body`, `in_reply_to`, `supersedes` |
| `delete` | `supersedes` | `body`, `in_reply_to`, `resolved` |

Notes on individual fields:

- `ts` is ISO 8601, UTC, **second** precision. It drives display order and last-writer-wins.
  Second precision is a known trade-off: two events in the same second have no defined causal
  order beyond the deterministic ID tie-break (an open question in the spec).
- `author` has the same semantics as git commit authorship — taken from `user.name` /
  `user.email`, unverified. A reserved `sig` field exists for future signing.
- `body` is CommonMark, always inline in the JSON (see the
  [storage deep dive](storage.md#why-bodies-are-inline) for why it must be).
- A `comment` carries no anchor. The thread's `anchor.json` does; the root comment is just
  the first message.

### Forward compatibility

Readers MUST ignore unknown fields and MUST preserve them (and events of unknown *type*) when
re-serializing. In the implementation this is a `#[serde(flatten)] extra` map on `Event` and
`Anchor`, and `Other(String)` variants on `EventKind` / `AnchorKind` — an event of type
`"reaction"` written by a future tool survives a round-trip through today's tool byte-for-byte.
`v` only bumps on semantic changes (fold rules, algorithm, canonicalization); additive fields and
new event types don't bump it.

## Canonical JSON and content-addressed IDs

An event's ID is `sha256(canonical_json(event))`, lowercase hex, truncated to 40 characters —
the same width as an abbreviated git object ID, comfortable to read and pass around. The
event's *filename* is its ID, and a thread's ID is the ID of its root comment.

For the hash to be reproducible everywhere, exactly one serialization must exist:

- UTF-8, no BOM, single line, no insignificant whitespace
- object keys sorted lexicographically (byte order)
- minimal string escaping; integers only, no floats, no leading zeros
- **the serialized bytes are exactly what is stored and exactly what is hashed** — there is no
  "parse then re-serialize" step anywhere that could drift

Content addressing is what makes the whole system coordination-free:

- **No ID allocation.** Offline writers can't collide: identical IDs mean byte-identical
  events, which are the *same* event and land at the same path. Storing it twice is a no-op.
- **Merges are set unions.** Two histories never disagree about a path's content (see the
  [sync deep dive](sync.md#the-union-merge)).
- **Duplicate publishes are idempotent.** Re-writing an event produces the identical blob at
  the identical path; the storage layer detects the unchanged tree and skips the commit.

The implementation pins this with a golden vector test: a fixed event must hash to a fixed ID,
independently computed. Any accidental change to canonicalization breaks it loudly.

## Anchors

One immutable `anchor.json` per thread:

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

| kind | has `path` | has `lines` | meaning |
|---|---|---|---|
| `commit` | no | no | about the whole change |
| `file` | yes | no | about one file's change |
| `range` | yes | yes | about specific lines |

The design records *precisely what the commenter saw*, so the anchor never needs to change:

- `diff.base` / `diff.head` — the two commits whose diff was reviewed. For a single-commit
  thread, `base` is the chosen parent of `head` (disambiguating merge commits). For a
  branch-level discussion, `base` is the merge-base *at the time of commenting* — pinning the
  exact diff even if the target branch moves later. Equal `base` and `head` are the *empty
  diff*: the thread annotates the state of `head` itself, not a change.
- `side` — `old` or `new`: which version of the file `path` and `lines` refer to. Comments on
  deleted lines anchor to the `old` side; comments on unchanged context lines use `new`.
- `lines` — 1-based, inclusive, **file coordinates on `side`**. Never diff/patch positions:
  patch positions are meaningless outside one specific rendering of one specific diff.
- `blob` — the ID of the exact file version on `side`. Technically redundant (derivable from
  the commit and path) but load-bearing twice over: it's an integrity check (if `head:path`
  doesn't resolve to this blob, the anchor is flagged, never silently rendered), and it's what
  [re-anchoring](reanchoring.md) derives its search snippet from.
- `old_path` — present iff the file was renamed within this diff.
- `cols` — reserved for sub-line spans; clients may ignore it today.

Anchors are always valid *against their own diff* — they cannot "break". Displaying a thread
on any other commit is re-anchoring, which is computed on demand and never stored.

## Why not a binary format?

Deliberately none of this is compressed, binary-encoded, or length-prefixed. Git packfiles
(zlib over delta chains of structurally similar JSON) are the compression layer and do well on
this data, and plain text keeps years of discussion `git grep`-able with no tooling at all.

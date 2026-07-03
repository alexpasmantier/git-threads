# git-threads

Anchored, threaded discussions stored inside a git repository — review comments that travel
with the code instead of living on a forge.

Threads are pinned to commits, files, or line ranges. All data lives on a dedicated ref
(`refs/threads/data`) that any git host stores without needing to know about it, syncs with
plain push/fetch, and can never produce a merge conflict. When code moves, threads follow it:
their position is recomputed against whatever commit you're looking at.

- **[How it works](docs/how-it-works.md)** — plain-language tour with diagrams, no git
  internals required
- **[Deep dives](docs/README.md)** — one document per subsystem: format, state fold, storage,
  sync, re-anchoring
- **[SPEC.md](SPEC.md)** — the format specification (draft v0.1)

## Status

Experimental. The format and CLI cover the core loop — comment, reply, edit, delete, resolve,
discard, show, list, and the git-shaped pull/commit/push cycle with drafts, session batching,
and re-anchoring — but the spec is a draft and may still change.
This repository dogfoods itself: its own review threads live on its `refs/threads/data`.

## Try it

```console
$ cargo install --path crates/git-threads   # puts `git-threads` on PATH; git finds it as `git threads`

$ git threads init                          # once per clone: fetch discussions too
$ git threads comment --file src/parser.rs --lines 120-128 \
    -m "does this handle empty input?"      # start a thread on HEAD's change
$ git threads list                          # threads, re-anchored to your checkout
$ git threads show <thread-id>              # code context + conversation
$ git threads commit                        # seal your drafts into local history
$ git threads push                          # share; safe under concurrent pushes
```

Reading the raw data needs nothing but git:

```console
$ git ls-tree -r --name-only refs/threads/data
$ git show refs/threads/data:threads/<xx>/<thread-id>/anchor.json
```

## Workspace

- `crates/git-threads-core` — the format: schemas, canonical JSON, content-addressed IDs,
  state fold, snippet matching. Pure logic, no I/O, no git dependency.
- `crates/git-threads` — the CLI: storage on the threads ref, sync, re-anchoring, commands.

## License

MIT

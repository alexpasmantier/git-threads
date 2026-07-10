# git-threads

Conversations about diffs. In plain git.

```text
$ git threads show bebce03de90b
thread bebce03de90bc053e93673edfce384fe1850da51  [resolved]
on crates/git-threads/src/main.rs:94-94 of 1a5ef68157e9..73e0f8733413
now crates/git-threads/src/main.rs:146-146 at 384dc4475f29 (fuzzy(3))

    143 │     }
    144 │ }
    145 │
>   146 │ fn main() -> anyhow::Result<()> {
    147 │     reset_sigpipe();
    148 │     let command = Cli::parse().command;
    149 │     if let Command::Mangen { out } = command {

● bebce03de90b  Jane Doe <jane@example.com> 2026-07-03T08:25:40Z
  Piping output into `head` panics with a broken-pipe error (os error 32): println! panics
  when stdout closes early. Standard fix is resetting SIGPIPE to default at startup (or
  handling ErrorKind::BrokenPipe). Found while dogfooding `show | head`.

↳ 825537036788  Jane Doe <jane@example.com> 2026-07-03T08:28:18Z
  Fixed in 0976b6f: SIGPIPE is reset to SIG_DFL at startup (unix only), so the process now
  exits quietly with 141 when the downstream pipe closes. Verified with `show | head`.
```

_The thread was pinned to line 94 at comment time; the code has since moved to line 146, and
`show` found it again by snippet matching (`fuzzy(3)`)._

Threads are pinned to commits, files, or line ranges. All data lives on a dedicated ref
(`refs/threads/data`) that any git host stores without needing to know about it, syncs with
plain push/fetch, and can never produce a merge conflict. When code moves, threads follow it:
their position is recomputed against whatever commit you're looking at.

- **[How it works](docs/how-it-works.md)** — plain-language tour with diagrams, no git
  internals required
- **[Command reference](docs/commands.md)** — every command: options, examples, behavior
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
$ git threads comment src/parser.rs:120-128 \
    -m "does this handle empty input?"      # start a thread on HEAD's change
$ git threads list                          # threads, re-anchored to your checkout
$ git threads show <thread-id>              # code context + conversation
$ git threads commit                        # seal your drafts into local history
$ git threads push                          # share; safe under concurrent pushes
```

Optional: install man pages so `git threads --help` works (git routes it to `man git-threads`):

```console
$ git threads mangen ~/.local/share/man/man1
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

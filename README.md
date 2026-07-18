# git-threads

Code comments document code. Commit messages document diffs — but only as a whole. `git-threads` brings the granularity back: comment on a commit, a file in its diff, or a hunk.

```text
$ git threads show bebce03de90b
thread bebce03de90bc053e93673edfce384fe1850da51 (resolved)
Original: crates/git-threads/src/main.rs:94-94 of 1a5ef68157e9..73e0f8733413
Current:  crates/git-threads/src/main.rs:146-146 at 384dc4475f29 (fuzzy(3))

    143 │     }
    144 │ }
    145 │
>   146 │ fn main() -> anyhow::Result<()> {
    147 │     reset_sigpipe();
    148 │     let command = Cli::parse().command;
    149 │     if let Command::Mangen { out } = command {

comment bebce03de90b  Jane Doe <jane@example.com>  Fri Jul 3 08:25:40 2026 +0000
    Piping output into `head` panics with a broken-pipe error (os error 32): println!
    panics when stdout closes early. Standard fix is resetting SIGPIPE to default at
    startup (or handling ErrorKind::BrokenPipe). Found while dogfooding `show | head`.

reply   825537036788  Jane Doe <jane@example.com>  Fri Jul 3 08:28:18 2026 +0000
    Fixed in 0976b6f: SIGPIPE is reset to SIG_DFL at startup (unix only), so the process
    now exits quietly with 141 when the downstream pipe closes. Verified with `show | head`.
```

_The thread was pinned to line 94 at comment time; the code has since moved to line 146, and
`show` found it again by snippet matching (`fuzzy(3)`)._

All data lives on a dedicated ref (`refs/threads/data`) that any git host stores without needing to know about it, syncs
with plain push/fetch, and can never produce a merge conflict. When code moves, threads follow it: their position is
recomputed against whatever commit you're looking at.

## How can I use this?

- PR discussions can be very valuable. let's keep these in the repository itself, where they survive a host migration and travel
  with every clone
- Commenting on a targetted change for future reviewers — or your future self
- Reviewing code without a forge: offline, over email, or on a bare git server
- Giving coding agents a place to leave and pick up review comments — they already speak git,
  no forge API needed
- Recording audit notes or code-archaeology findings pinned to the commits they concern
- Making a git repository a self-contained knowledge base, with discussions that follow the code
  as it moves and changes

## Status

Experimental. The format and CLI cover the core loop — comment, reply, edit, delete, resolve,
move, discard, show, list (with `--grep` search and `--json` output), status, a local inbox
(`list --new`), the git-shaped pull/commit/push cycle with drafts, session batching,
and re-anchoring, plus a GitHub importer that liberates PR review history into the
repository — but the spec is a draft and may still change.
This repository dogfoods itself: its own review threads live on its `refs/threads/data`.

## Try it

```console
$ cargo install --path crates/git-threads   # puts `git-threads` on PATH; git finds it as `git threads`

$ git threads init                          # once per clone: fetch discussions too
$ git threads comment src/parser.rs:120-128 \
    -m "does this handle empty input?"      # start a thread on HEAD's change
$ git threads comment main...topic src/parser.rs:120 \
    -m "same, on a branch's whole diff"     # ranges work like git diff
$ git threads list                          # threads, re-anchored to your checkout
$ git threads list main...topic --open      # what still needs attention on this branch
$ git threads list --new                    # what did I miss?
$ git threads show <thread-id>              # code context + conversation
$ git threads status                        # drafts, unpushed events, new activity
$ git threads commit                        # seal your drafts into local history
$ git threads push                          # share; safe under concurrent pushes

$ git threads import github 123             # a PR's review threads, or --all (needs gh)
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

## License

MIT

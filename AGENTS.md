# Working agreement: ps-vello

Operating contract for any coding agent here, and the single source of truth. Codex,
Cursor and Gemini read `AGENTS.md` natively; Claude Code imports it from `CLAUDE.md`.
Never fork these rules into a per-vendor file.

## What this repository is

A PathScale fork of [linebender/vello](https://github.com/linebender/vello), the GPU
renderer `ps-anyrender-vello` drives and `ps-blitz` paints through.

It exists for two reasons:

1. **Memory.** Upstream sizes its compute buffers from constants "hand picked to
   accommodate the vello test scenes as well as paris-30k", with a comment saying they
   "should instead get derived from the scene layout". They never were, so every
   renderer instance reserved about 165 MB whether it drew a chat window or a city map.
   Measured on AgencyZero: 52 pooled 8 MB blocks, 416 MB, held for the life of the
   process.
2. **crates.io refuses git dependencies.** Publishing anything above this in the stack
   means publishing this too.

**Every published crate is renamed, the Rust names are not.** `package.name` is
`ps-vello`; `lib.name` stays `vello`. So `use vello::...` is unchanged in every
consumer, and only the manifests differ from upstream. Keep it that way: renaming the
lib names would turn a manifest change into a rewrite and make every future upstream
merge a conflict.

## Remotes

- `origin` is `git@github.com:pathscale/ps-vello.git`. This is where work goes.
- `upstream` is `https://github.com/linebender/vello.git`, fetch only. Never push there.

## Staying close to upstream

The fork's value is that it is boring. Carry local changes as a thin layer:

- Do not reformat, rename modules, or restructure.
- Prefer taking a fix from upstream over writing our own.
- When merging upstream, expect conflicts in the robust-memory path. If a conflict
  reaches a shader that the port did not touch, take upstream's side.

## The robust memory port

`port/robust-memory` carries upstream [#606](https://github.com/linebender/vello/pull/606)
(which fixes [#366](https://github.com/linebender/vello/issues/366)), squashed onto
master. The pipeline reads back the bump buffer from two frames ago and sizes the next
frame from what the scene actually used. Initial sizes are 6.5 MB rather than 165 MB and
grow on demand.

Starting small is safe because the shaders already flag overflow through `bump.failed`:
a frame that outgrows its buffers reports it, and the next frame is reallocated.

That PR is still open upstream. If it lands, drop this port and take theirs.

## Tests

`cargo test --workspace` passes except `vello_sparse_tests`, which fails identically on
clean upstream `main`: its snapshot PNGs come from git-lfs and are not present. 2,664
failures there are the expected state, not a regression. Check against `main` before
believing otherwise.

## Publishing

Bottom-up, because a crate cannot be published until its dependencies are on the
registry:

    ps-vello-encoding
    ps-vello-shaders
    ps-vello

Dry-run first (`cargo publish -p <crate> --dry-run`), never publish from a dirty tree,
and remember that a published version can only be yanked, never replaced.

## Git workflow

- Work on a branch, ship through a pull request, never commit to `main`.
- **Never squash. Rebase-merge only.** The one exception is porting somebody else's WIP
  branch, as the robust-memory port did: 18 commits with messages like "Fixup handling
  to not be busted" are not a history worth keeping, and that call is the owner's.
- **Never create merge commits**, not even to refresh a branch. Rebase and
  force-with-lease.
- No AI attribution anywhere: no `Co-Authored-By`, no "Generated with" footer.
- No em dashes in commit messages. Comma, colon, parentheses or a full stop.

## Before delivery

    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace

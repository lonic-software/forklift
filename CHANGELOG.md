# Changelog

Notable changes to Forklift, for people who use it. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Before 1.0, any release can change commands,
on-disk formats and the wire protocol, with no migration.

Entries are written by hand in the pull request that makes the change, under **Unreleased**. What
earns one is listed in the [contributor guide](docs/guide/contributing.md#8-keeping-docs-in-sync).
`bin/release` turns that section into the new version and refuses a release if it is empty.

## [Unreleased]

### Changed

- **The command surface was redesigned, with no aliases for the old names.** Families get subcommands:
  `pallet create/list/delete/restore`, `store status/compact/peek/evict`,
  `git import/export/mirror`, `scope fetch/narrow/prune/status`, `self alias/schema/update`,
  `config list/get/set/unset`, `manifest set/get/unset/keys/log`. `franchise` is now `clone`,
  `cherry-pick` is `pick`, `haul merge` is `haul consolidate`, and `office keygen` moved to
  `profile keygen`. Recorded text is always `-m`. Unique prefixes of command names work. See
  `docs/generated/commands.md`.
- Destructive operations preview by default and act with `--confirm`; without it they exit 39.
- **`lower` keeps your local changes.** It no longer needs a clean warehouse: staged and unstaged
  changes the remote's new parcels leave alone come through the lower as they were. A lower that
  would write, remove or replace a path holding a local change, or a folder above or below one,
  refuses, names those paths and changes nothing. During a consolidation or pick in progress, a
  lower is refused unless the bay is already up to date.
- **`lower` merges a diverged remote.** When your pallet and the remote's both hold new parcels,
  `lower` merges the remote's head in and stacks the merge, and `lift` publishes it: two commands
  where there used to be five and a leftover pallet. A merge that conflicts stops with
  `merge_conflicts` (exit 47) and the consolidation in progress; `stack` completes it and `abort`
  abandons it. Changes to tracked files refuse the merge with `conflict` (exit 4), as they
  now refuse `consolidate`, `pick`, `haul consolidate`, `shift`, an `undo` of a shift and
  `pallet create` at a revision. `lower --repair` merges its repair the same way instead of
  stacking it on a pallet of its own.
- `restore --staged` is gone; use `unload`.
- MCP tools are generated from the command tree.
- `--json` documents are now at schema 21.
- **Ignore files speak gitignore.** Every line of `.forkliftignore` is a gitignore line, and every
  `.gitignore` in the tree is honoured where it stands; `.forkliftignore` is read last and wins over
  them. A regex is written as a `re:` or `!re:` line, matched against the whole path. Matching is
  case-sensitive on every system. A `.git` folder or file is ignored at any depth by a rule no file
  can remove, so `git import` no longer writes one into `.forkliftignore`. `stocktake` reports an
  untracked folder only when `load .` would stage something in it. A `.forkliftignore` line that
  reads as an old regex, starting with `^` or ending with `$`, is refused rather than misread.
- **A sparse checkout ignores what a full one ignores.** Besides the root `.forkliftignore`, every
  `clone --only` scope and scoped bay now holds the root `.gitignore` and the `.gitignore` of each
  folder above a scoped path, and `scope narrow` removes the ones it no longer needs. Sparse
  warehouses and scoped bays made before this change are refused and must be made again.
- A failure printed for a person shows its next step on its own labelled line, in both heads, and the
  message no longer restates it.
- **A warehouse now records its format, and a build reads exactly one.** A warehouse in another format
  is refused before anything in it is read, with its own error code and way out. Warehouses made by
  an earlier build are refused and must be made again: no upgrade reaches back before this change.
- **The AWS head records each warehouse's format too**, in a DynamoDB item every request reads first,
  and refuses a warehouse in another format with a `503`. Every ref and trust write is checked against
  it, so no write lands once an upgrade has begun. Warehouses written by an earlier deployment are
  refused and must be made again under a new id.
- The parked list, a pick in progress and the trust file now record their version, and an object
  stored at a version this build does not know is refused by name rather than read as damage. Such
  files written by an earlier build are refused.
- Forklift records no display names: `operator.name`, the profile name and `--display-name` are gone.

### Added

- **`forklift abort` abandons a consolidation or pick that stopped at conflicts.** The files the merge
  touched go back to the pallet head and nothing is left in progress; changes to other files are kept.
  It refuses without `--confirm` (exit 39) where it would throw away what you changed since, such as a
  resolution. `undo` right after such a consolidate or pick now aborts it, where it used to drop the
  record of the merge and leave the files it wrote, conflict markers included. Removing `.forklift/consolidation` or `.forklift/cherry-pick` by
  hand is no longer the way out, except where the abort cannot run, and those refusals say so.
- `forklift upgrade` and `forklift-server upgrade --root` upgrade a warehouse, with all its bays, to the
  format the build reads, and resume an upgrade that was interrupted. Both refuse while anything else has
  the warehouse open (exit 45). No format before the first is upgraded.
- `forklift-aws-upgrade` upgrades the warehouses of an AWS deployment in place, from an operator's
  machine under the new `iam/operator.policy.json`, and resumes an interrupted run with `--takeover`.
- Partial warehouses: a cache warehouse fetches what it reads from its origin, `store evict` frees what
  the origin holds, and `remote move` moves the origin.
- `git mirror` keeps a git remote in step, never by force.
- Pallets can be deleted and restored, locally and on a remote, on both server heads.
- A cut-off `clone` keeps what it fetched and resumes.
- A tutorial, a concepts page and a glossary in `docs/guide`.
- `--color`, and `NO_COLOR` is honoured.
- A remote refuses a pallet head holding a name no client can check out, `lower` and `clone` refuse
  one a remote already holds, and `lower --repair` and `clone --repair` stack a way back from it.
- A remote pallet delete names the head it deletes, `--at` pins it to that head, and the remote
  listing shows each pallet's head.

### Fixed

- Every parcel `stack` and `pick` record has a description. `stack` without `-m` refuses instead of
  recording a parcel with none, unless it completes a consolidation or pick in progress, which
  supplies one. `pick` of a parcel with no description needs `-m`. A blank `-m` on `stack`, `pick`
  or `deliver` is refused.
- `forklift-server` authenticates from a request's headers before reading its body, so an
  unauthenticated peer can no longer hold server memory by opening large uploads.
- A served root's meta pallets never move back to an ancestor.
- Every `.forklift` folder ignores itself for git, so `git add -A` in a colocated repository never
  stages a remote token.
- Many silent-loss, case-folding, durability and trust-verification fixes. Pull request titles from
  #152 to #343 have the detail.

## [0.3.1] - 2026-10-16

### Added

- `self-schema` describes the command surface as a Pult manifest.
- The `lonic_json` envelope; usage errors are documents under `--json`.
- Progress events and MCP progress notifications for long operations.

### Changed

- `trust_changed` refusals carry details, and `accept-regenesis --confirm` binds the value.

### Fixed

- Many fixes to `heal`, garbage collection and config writes.

## [0.3.0] - 2026-07-27

### Added

- An OpenTofu reference deployment for the AWS serverless head.
- `heal` recovers vanished objects from the remote and no longer wedges in sparse warehouses.

### Fixed

- `compact` verifies content on the way into a pack and skips corrupt objects instead of aborting.
- An orphaned pack index no longer wedges the warehouse.

## [0.2.1] - 2026-07-14

### Added

- A verified query engine over parcels.
- Reaching a remote over Tor, with per-remote SOCKS routing for `.onion` addresses.

## [0.2.0] - 2026-07-13

### Added

- Sparse warehouses: fetch only what a task needs, and forget what it no longer does.
- `show` prints one file's content at a revision.

### Changed

- The AWS head is locked by default.

## [0.1.7] - 2026-07-09

### Added

- The `fl` short alias.

## [0.1.6] - 2026-07-08

### Changed

- Repository history was restarted. Tags before this one are not ancestors of later ones.

## [0.1.5] - 2026-07-07

### Added

- `store`, an object-store health command.

### Changed

- Every object read verifies its Blake3 hash, and writes are durable before anything destructive
  across power loss.

## [0.1.4] - 2026-07-07

### Changed

- `compact`, `consolidate`, `diff` and `audit` run across all cores, and reads got a content-addressed
  cache.

## [0.1.3] - 2026-07-07

### Added

- The AWS serverless head.

### Fixed

- `import-git` no longer crashes on non-UTF-8 history.

## [0.1.2] - 2026-07-05

### Fixed

- Self-update pins the resolved version and no longer reports success it did not have.

## [0.1.1] - 2026-07-05

### Added

- Hauls: pull requests with signed reviews.
- Git-compatible command aliases, and a Dockerfile for `forklift-server`.

## [0.1.0] - 2026-07-05

First release: signed parcels, bays, journaled `undo`, `import-git` and `export-git`, optimistic lift
and self-update.

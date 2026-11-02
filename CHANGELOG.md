# Changelog

Notable changes to Forklift, for people who use it. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Before 1.0, any release can change commands,
on-disk formats and the wire protocol, with no migration.

Entries are written by hand in the pull request that makes the change, under **Unreleased**.
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
- `restore --staged` is gone; use `unload`.
- MCP tools are generated from the command tree.
- `--json` documents are now at schema 15.
- Forklift records no display names: `operator.name`, the profile name and `--display-name` are gone.

### Added

- Partial warehouses: a cache warehouse fetches what it reads from its origin, `store evict` frees what
  the origin holds, and `remote move` moves the origin.
- `git mirror` keeps a git remote in step, never by force.
- Pallets can be deleted and restored, locally and on a remote, on both server heads.
- A cut-off `clone` keeps what it fetched and resumes.
- A tutorial, a concepts page and a glossary in `docs/guide`.
- `--color`, and `NO_COLOR` is honoured.

### Fixed

- `forklift-server` authenticates from a request's headers before reading its body, so an
  unauthenticated peer can no longer hold server memory by opening large uploads.
- Many silent-loss, case-folding, durability and trust-verification fixes. Pull request titles from
  #152 to #327 have the detail.

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

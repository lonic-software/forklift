# Forklift
A fun VCS written in Rust.

The idea for Forklift was born when I tried to design a serverless hosting provider for Git repositories.\
At the end of my research, I came to the conclusion that Git just simply wasn't designed for this.\
So, I decided to build my own VCS with this in mind, and also add some other nice features along the way.

## Documentation

Detailed guides live in [`docs/guide/`](docs/guide/README.md):

- **[User guide](docs/guide/cli.md)** — every CLI command, organized by workflow, with examples.
- **[Trust & identity](docs/guide/trust-and-identity.md)** — signing, users, roles, agents, key protection.
- **[Peer-to-peer over Tor](docs/guide/p2p-tor.md)** — work on a warehouse with friends with no server, over Tor onion services.
- **[Machine & agent interface](docs/MACHINE_INTERFACE.md)** — `--json`, stable errors, the MCP server.
- **[VCS comparison](docs/COMPARISON.md)** — honest comparison of Forklift vs. git, jujutsu, sapling, and pijul.
- **[Running a server](docs/SERVER.md)** — self-host a forklift remote.
- **[Contributing](docs/guide/contributing.md)** — architecture, build/test, conventions.
- **[Design & roadmap](docs/DESIGN.html)** — the source of truth for how and why.

## Install

**With [pult](https://github.com/lonic-software/pult)** — an npx-style launcher for a repo's operational commands. One command fetches the installer and lets you pick the CLI, the server, or both:

```sh
pult x github.com/lonic-software/forklift install
```

**Without pult** — a one-liner:

```sh
# macOS / Linux / Git Bash — installs the `forklift` CLI to ~/.local/bin
curl -fsSL https://raw.githubusercontent.com/lonic-software/forklift/main/install.sh | sh

# the server head instead (or `all` for both):
curl -fsSL https://raw.githubusercontent.com/lonic-software/forklift/main/install.sh | sh -s -- server
```

```powershell
# Windows (PowerShell)
irm https://raw.githubusercontent.com/lonic-software/forklift/main/install.ps1 | iex
# server head: set $env:FORKLIFT_COMPONENT="server" (or "all") before piping
```

**From source:** `cargo install --path crates/forklift` (or `cargo build --release`).

## Quick start

```sh
forklift prepare                          # create a warehouse here
forklift config operator.name "Ada"       # optional; an id is minted otherwise
echo "hello" > readme.txt
forklift load .                            # stage everything
forklift stack "first parcel"             # commit
forklift history                           # see it
```

## Collaborate peer-to-peer — no server

Work on a warehouse with a few friends without a hosted server, a fixed IP, or port-forwarding.
One command publishes it as a [Tor](https://www.torproject.org/) onion service and prints the one
thing to share:

```sh
forklift peer
#   address   http://abcd…xyz.onion
#   token     3f7c1e90-…
```

Your friend clones it, and you both push and pull over Tor:

```sh
forklift franchise http://abcd…xyz.onion myproject --token 3f7c1e90-…
forklift lift        # push over Tor
forklift lower       # pull over Tor
```

Every parcel is signed and content-addressed, so you trust the history, not the transport. It
needs a local `tor` and the server head installed (`install.sh` with `all`). Full walkthrough,
including a stable address and the Tor setup: **[Peer-to-peer over Tor](docs/guide/p2p-tor.md)**.

## GUIs

Prefer a graphical client? Community-built GUIs for Forklift:

- **[forklift_ui](https://github.com/r4nd0mth1ngs/forklift_ui)** by [r4nd0mth1ngs](https://github.com/r4nd0mth1ngs) — a cross-platform desktop app built with Tauri and Rust.

Built one? Open a PR to add it here. GUIs should drive Forklift through the
[machine & agent interface](docs/MACHINE_INTERFACE.md) (`--json`, stable errors) rather than
scraping CLI output.

## The name
Why is it called Forklift, you ask? Well, it's easy.. Forklifts are used in warehouses to move and organize packages.\
This tool does exactly that, but with files on a computer.

## Terminology
I wanted Forklift to have a little soul, so I built the whole terminology around forklifts.\
Hopefully it is easily understandable and not hard to remember.

### Concepts
#### Warehouse
Git counterpart: Repository\
A warehouse is where all the files of a given project live.

#### Parcel
Git counterpart: Commit (noun)\
A parcel is a set of changes.

#### Pallet
Git counterpart: Branch\
A pallet is an independent line of development. Parcels are stacked on top of each other on a pallet.\
You can have separate versions of your project on separate pallets.\
It is recommended to develop each feature or fix in its own pallet, and then merge it into the main pallet
(the main version of your project).

#### Fork
Git counterpart: Fork\
A fork is a copy of a warehouse. When you want to create a custom version of someone else's project,
you can fork their warehouse and implement your changes in your own copy.\
Forks can also be merged into the original warehouse.

### Actions
#### Prepare
Git counterpart: Init\
Prepare a new warehouse. This is the first step when initializing a project.

#### Palletize
Git counterpart: Checkout to new branch\
Create a new pallet to organize a set of changes.

#### Shift
Git counterpart: Checkout\
Move to a different pallet.

#### Load
Git counterpart: Add\
Add a set of changes to a parcel.

#### Unload
Git counterpart: Remove\
Remove a set of changes from a parcel.

#### Stack
Git counterpart: Commit (verb)\
Stack a parcel (add changes) on top of the current pallet.

#### Consolidate
Git counterpart: Merge\
Combine changes from one pallet into another (warehouse workers consolidate loads onto one pallet).

#### Cherry-pick
Git counterpart: Cherry-pick\
Apply one parcel's change onto the current pallet as a new, author-preserving, freshly-signed
parcel. Only adds — no rewrite, no force-push.

#### Stocktake
Git counterpart: Status\
Audit the warehouse: report the current pallet, the staged changes and the changes not yet loaded.

#### Office
Git counterpart: none (GitHub org/keys settings, at best)\
Manage the warehouse office: users and signing keys, tracked as metadata on the reserved
`office` pallet. Enrolling establishes trust — from then on every parcel is signed (Ed25519),
and that cannot be undone.

#### Audit
Git counterpart: none (`git verify-commit`, at best)\
Verify the signed history offline: the office chain back to the genesis parcel, then the
parcels of a pallet.

#### Tag
Git counterpart: Tag\
Mark a parcel with a named, signed pointer — a release. The tagger is the parcel's signature
(forge-proof), verifiable offline against the office chain; cutting one requires an admin.

#### Diff
Git counterpart: Diff\
Show the changed lines: working directory vs inventory by default, inventory vs pallet head
with `--staged`, or the heads of two pallets with `diff <pallet-a> <pallet-b>`.

#### History
Git counterpart: Log\
Walk the parcels of a pallet, newest first: hash, operators, timestamps and description.

#### Blame
Git counterpart: Blame\
Attribute each line of a file to the parcel — and signed author — that introduced it. Because
authorship carries an identity class (§7.1), it answers "was this line written by a human or an
agent, under whose supervision" — blame git cannot express.

#### Restore
Git counterpart: Restore\
Discard changes in the working directory (or, with `--staged`, unstage changes from the inventory).

#### Lift
Git counterpart: Push\
Upload changes to a remote warehouse.

#### Lower
Git counterpart: Pull\
Download changes from a remote warehouse to your local copy.

#### Franchise
Git counterpart: Clone\
Open a local franchise of a remote warehouse: a full local copy that keeps syncing
with the original through lift and lower.

#### Haul
Git counterpart: Pull request\
Request to merge changes from one pallet into another.

#### Park
Git counterpart: Stash\
Temporarily set aside changes that aren't ready to be stacked onto a pallet.

## License
Forklift is **open-core**. See [LICENSING.md](LICENSING.md) for the full map; in short:

- **The client** — the `forklift` command-line tool and the `forklift-core` library (plus the
  docs and formats) — is open source, licensed under either of
  - Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or https://www.apache.org/licenses/LICENSE-2.0)
  - MIT license ([LICENSE-MIT](LICENSE-MIT) or https://opensource.org/licenses/MIT)

  at your option. Use it, self-host with it, and build on it freely.
- **The server heads** — `forklift-server` and `forklift-aws-lambda` — are source-available
  under the [Functional Source License 1.1](LICENSE-FSL) (`FSL-1.1-ALv2`). You may do anything
  with them *except* offer a commercial service that competes with Forklift's own hosting;
  self-hosting for your own use is free. Each released version becomes Apache-2.0 two years
  after its release.

### Contribution
Unless you explicitly state otherwise, any contribution to the **client** (`forklift-core`,
`forklift`) intentionally submitted for inclusion in Forklift by you, as defined in the
Apache-2.0 license, shall be dual licensed as MIT/Apache above, without any additional terms
or conditions. Contributions to the **server heads** are under FSL-1.1 and (once a CLA is in
place) require it — see [LICENSING.md](LICENSING.md).

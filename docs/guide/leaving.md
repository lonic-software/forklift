# Leaving Forklift: taking a project to git

Trying Forklift should never trap a project. `forklift git export` writes a warehouse's history into
a new git repository: parcels become commits, pallets become branches and tags become annotated
tags, and the same history always exports to the same commit hashes. This page walks through
leaving, including the parts that need a step of their own: work that is only on the server, and
the records git has nowhere to keep.

As in the [tutorial](tutorial.md), each `$` line is a command and the lines under it are what
forklift prints. A `…` stands for text that differs on your machine. A word in angle brackets, such
as `<haul>`, stands for a value forklift prints that a later command uses: copy it from your own
output. The test suite runs this page (`crates/forklift/tests/integration/guide_pages.rs`), so it
cannot drift from the CLI. You need `git` on your PATH.

## 1. A project shared through a server

To follow along, make a small project with two pallets and a haul, shared through a server. With a
project of your own, skip to section 2.

```console
$ forklift config set operator.identifier ada@example.com --global
$ forklift-server prepare --root server
Prepared warehouse at "server".
```

Open a **second terminal** in the same folder and start the server there. Leave it running:

```console
$ forklift-server serve --root server --addr 127.0.0.1:7171 --open
forklift-server listening on http://127.0.0.1:7171
```

Back in the first terminal, make the project, lift `main`, then lift a `feature` pallet with a haul
proposing it:

```console
$ mkdir project
$ cd project
$ forklift prepare
Prepared warehouse.
$ echo 'Our project.' > readme.md
$ forklift load readme.md
$ forklift stack -m "Start the project"
Stacked parcel … on pallet "main".
$ forklift config set remote.url http://127.0.0.1:7171
$ forklift lift
Lifted pallet "main" to …: … new parcel(s), … object(s) and 0 signature(s) uploaded.
$ forklift pallet create feature
Created pallet "feature" and shifted to it.
$ echo 'A feature.' > feature.md
$ forklift load feature.md
$ forklift stack -m "Add the feature"
Stacked parcel … on pallet "feature".
$ forklift haul open --target main --source feature --title "Add the feature" -m "Ready for review"
Opened haul … — "Add the feature".
  feature → main
$ forklift lift
Lifted the @haul pallet.
Lifted pallet "feature" to …: … new parcel(s), … object(s) and 0 signature(s) uploaded.
$ cd ..
```

## 2. Export, and read what it says it left behind

Leave from any copy of the warehouse. Here it is a fresh clone, which holds only `main`:

```console
$ forklift clone http://127.0.0.1:7171 copy
Adopted the @haul pallet from the remote.
Cloned http://127.0.0.1:7171 into "copy": pallet "main" at … (… object(s) fetched loose).
$ cd copy
$ forklift git export ../project-git
Exported 1 commit(s) into a git repository at "../project-git" (1 branch(es), 1 tree(s), 1 blob(s)).
Checked out "main".
  warning: the export has no branch for the remote's pallet "feature" as it stands there; "forklift lower feature" brings it here.
  Then export again, into a new path.
Not exported, as git has no equivalent: @haul (hauls and their reviews).
```

An export carries only what this warehouse holds, so it asks the configured remote what it has
besides, and names each pallet it has no branch for as it stands there. If the remote cannot be reached, the
export still finishes and says it could not check; `forklift pallet list --remote` asks again later.

## 3. Bring everything here, then export again

List the remote's pallets, lower each one the export named, and export into a new path:

```console
$ forklift pallet list --remote
Pallets on http://127.0.0.1:7171:
  feature at … (not here; "forklift lower feature" brings it in)
  main at … (same as here)
$ forklift lower feature
Created pallet "feature" at the remote's head … (… object(s) and 0 signature(s) fetched); "forklift shift feature" moves this bay onto it.
$ forklift git export ../project-git-all
Exported 2 commit(s) into a git repository at "../project-git-all" (2 branch(es), 2 tree(s), 2 blob(s)).
Checked out "main".
Not exported, as git has no equivalent: @haul (hauls and their reviews).
```

Nothing is left behind on the remote now. Work you have not stacked is not history yet, so it is not
exported either: stack it first. Parked work (`forklift park`) is on no pallet; the export counts it
in a warning, and `forklift park pop` brings it back to stack.

## 4. Keep what git has no home for

Git has no place for Forklift's metadata, so the export names what it leaves out: the signed office
(`@office`), approvals and provenance (`@manifest`), hauls and their reviews (`@haul`), custom
metadata (`@kv`) and the signatures on parcels and tags. Save the hauls as JSON before you go. The
list names every haul, and `haul show` holds each one's reviews and comments:

```console
$ forklift haul list --state all
<haul>  [open]  Add the feature  (feature → main)
$ forklift haul list --state all --json > hauls.json
$ forklift haul show <haul> --json > haul-<haul>.json
```

The signatures can only be checked by Forklift. If you may need to prove later who stacked what, keep
the warehouse folder: `forklift audit` checks it offline, with no server.

Everything else is in git. Each commit message ends in a `Forklift-Parcel:` line naming the parcel it
came from, so a commit can always be traced back to the warehouse you kept.

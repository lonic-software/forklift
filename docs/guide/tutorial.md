# Tutorial: from an empty folder to a shared warehouse

This walks one small project through the whole forklift workflow in about fifteen minutes: create a
warehouse, record changes, branch and merge (with a conflict), set work aside, sign your history,
and share it through a server. Each step names the reference section that covers it in full.

Type the commands as you go. Each `$` line is a command and the lines under it are what forklift
prints. A `…` stands for text that differs on your machine, such as hashes, ids and dates. The
commands are written for a POSIX shell:
the macOS or Linux terminal, or Git Bash on Windows.

The test suite runs this page. `crates/forklift/tests/tutorial.rs` executes every command block
below, in order, and fails if any command fails or prints something other than what is shown here,
so the page cannot drift from the CLI.

## 1. Before you start

Install the `forklift` CLI and the `forklift-server` head (see [Install](../../README.md#install)),
then check that the CLI runs:

```console
$ forklift --version
forklift …
```

You need no account and no configuration. Forklift identifies you by an opaque **operator id**,
minted the first time you record anything. One machine can act as several operators, each with its
own id: see `profile` under [`config`](cli.md#config--who-you-are-and-settings).

## 2. Create a warehouse

A **warehouse** is a repository. Make a folder for the project and prepare it:

```console
$ mkdir hello
$ cd hello
$ forklift prepare
Prepared warehouse.
```

Add a file, and a build folder that should never be tracked. A `.forkliftignore` at the warehouse
root holds one regular expression per line, matched against `/`-separated paths:

```console
$ echo "Hello, world!" > readme.md
$ mkdir build
$ echo "compiled output" > build/app.bin
$ echo '^build(/.*)?$' > .forkliftignore
```

`stocktake` is forklift's `status`. The build folder does not appear in it:

```console
$ forklift stocktake
On pallet "main" (unborn — nothing stacked yet)

The inventory matches the pallet head; nothing is staged.

Changes not in the inventory (use "load" to stage them):
  untracked: .forkliftignore
  untracked: readme.md
```

## 3. Record changes

`load` stages changes into the **inventory**, and `stack` records the inventory as a **parcel**, a
commit. `.forkliftignore` is ordinary tracked content, so its rules travel with the history.

```console
$ forklift load .
$ forklift stack "First parcel"
Stacked parcel … on pallet "main".
```

Your operator id now exists:

```console
$ forklift config operator.identifier
…
```

Change the greeting. `diff` shows the change line by line before you stage it:

```console
$ echo "Hello, forklift!" > readme.md
$ forklift diff
modified: readme.md
  1   - Hello, world!
    1 + Hello, forklift!
$ forklift load readme.md
$ forklift stack "Greet forklift"
Stacked parcel … on pallet "main".
```

`history` walks the parcels, newest first:

```console
$ forklift history
parcel …
author … at …
stack  … at …

    Greet forklift

parcel …
author … at …
stack  … at …

    First parcel
```

Reference: [Tracking changes](cli.md#2-tracking-changes) and [`history`](cli.md#history--the-log-hi).

## 4. Branch and merge

A **pallet** is a branch. `palletize` creates one and shifts to it:

```console
$ forklift palletize shout
Created pallet "shout" and shifted to it.
$ echo "HELLO, FORKLIFT!" > readme.md
$ forklift load readme.md
$ forklift stack "Shout the greeting"
Stacked parcel … on pallet "shout".
```

`shift` switches back to `main`, which still has the old line. Change the same line there too:

```console
$ forklift shift main
Shifted to pallet "main" (head …).
$ cat readme.md
Hello, forklift!
$ echo "Hello, forklift and friends!" > readme.md
$ forklift load readme.md
$ forklift stack "Greet friends too"
Stacked parcel … on pallet "main".
```

Now `consolidate`, forklift's `merge`, brings `shout` in. Both pallets changed the same line, so it
stops with a conflict and writes both sides, and their common base, into the file:

```console
$ forklift consolidate shout
Consolidating "shout" into "main" produced 1 conflict(s):
  conflict: readme.md

Resolve the conflicts, "load" the resolved files, then "stack" to complete the consolidation.
$ cat readme.md
<<<<<<< main
Hello, forklift and friends!
||||||| base
Hello, forklift!
=======
HELLO, FORKLIFT!
>>>>>>> shout
$ forklift conflicts
Unresolved conflicts (1):
  readme.md (base …, ours …, theirs …)

Resolve each file, "load" it, then "stack" to complete the consolidation.
```

Resolve it by writing the file you want, then load and stack. The stack completes the
consolidation as a parcel with two parents:

```console
$ echo "HELLO, FORKLIFT AND FRIENDS!" > readme.md
$ forklift load readme.md
$ forklift stack "Consolidate shout"
Stacked parcel … on pallet "main".
$ forklift history --limit 1
parcel …
consolidates … + …
author … at …
stack  … at …

    Consolidate shout
```

Reference: [Branching and merging](cli.md#4-branching-and-merging).

## 5. Set work aside, and take a step back

`park` is forklift's `stash`. It saves your changes to tracked files and resets the folder to the
pallet head, and `park pop` brings them back, staged:

```console
$ echo "A half-finished idea" >> readme.md
$ forklift park
Parked the work in progress as … and reset to the pallet head.
$ forklift stocktake
On pallet "main" (head …)

The inventory matches the pallet head; nothing is staged.

The working directory matches the inventory.
$ forklift park pop
Re-applied the parked changes from … (staged).
```

Suppose you stack the half-finished idea by mistake. `undo` reverses the last operation; after
undoing a stack, the parcel's changes are staged again:

```console
$ forklift stack "Half-finished idea"
Stacked parcel … on pallet "main".
$ forklift undo
Undid stack on pallet "main".
  (Half-finished idea)
The pallet head is now ….
Its changes are staged again — "stack" to redo, or adjust them first.
```

Park the idea again for later; `park list` shows what is parked:

```console
$ forklift park
Parked the work in progress as … and reset to the pallet head.
$ forklift park list
… Parked changes on pallet "main".
```

Reference: [`park`](cli.md#park--stash-pa) and [`undo`](cli.md#undo--reverse-the-last-operation).

## 6. Sign your history

`office enroll` makes you the warehouse's first operator, generates your signing key, and starts
the **office**, the signed record of who may contribute. From then on every parcel stacked here
must be signed. Enrolling cannot be undone.

```console
$ forklift office enroll
Enrolled "…" and established trust.
Genesis parcel:    …
Identity root key: …

From now on every parcel stacked in this warehouse must be signed. This cannot be undone (a lockout is handled by archiving and re-preparing the warehouse).
$ echo "Signed from here on." > notes.md
$ forklift load notes.md
$ forklift stack "Add notes"
Stacked parcel … on pallet "main".
```

`audit` verifies the signatures offline. The parcels stacked before you enrolled stay as they were,
unsigned:

```console
$ forklift audit
Office chain verified back to genesis ….
Pallet "main" verified: 1 signed parcel(s) valid, 5 legacy parcel(s) predate trust and are unsigned.
```

Reference: [Trust and identity](trust-and-identity.md).

## 7. Share it through a server

Go up to the folder that holds `hello`, and prepare an empty warehouse for a server to serve:

```console
$ cd ..
$ forklift-server prepare --root server
Prepared warehouse at "server".
```

Open a **second terminal** in the same folder and start the server there. Leave it running. With
`--open` it accepts anyone, which is fine on your own machine and nowhere else (see
[Running a server](../SERVER.md) for tokens):

```console
$ forklift-server serve --root server --addr 127.0.0.1:7171 --open
forklift-server listening on http://127.0.0.1:7171
```

Back in the first terminal, point `hello` at the server and `lift` (push) it:

```console
$ cd hello
$ forklift config remote.url http://127.0.0.1:7171
$ forklift lift
Lifted the office pallet: 1 new parcel(s).
Lifted pallet "main" to …: … new parcel(s), … object(s) and 1 signature(s) uploaded.
```

`franchise` (clone) a second copy beside it, as a colleague would. The first contact with a server
adopts its trust anchor and warns you to check it; `--expect` checks it before anything is
fetched:

```console
$ cd ..
$ forklift franchise http://127.0.0.1:7171 hello-copy
Warning: FIRST CONTACT with http://127.0.0.1:7171: adopted its trust anchor, genesis …. Verify that genesis out of band. To check it before anything is fetched, pass --expect … to franchise or lower.
Adopted the remote's trust anchor (genesis …); every parcel is signed from now on.
Franchised http://127.0.0.1:7171 into "hello-copy": pallet "main" at … (… object(s) fetched loose).
```

Make a change in the copy and lift it:

```console
$ cd hello-copy
$ echo "Edited in the copy." >> notes.md
$ forklift load notes.md
$ forklift stack "Edit notes in the copy"
Stacked parcel … on pallet "main".
$ forklift lift
Lifted pallet "main" to …: 1 new parcel(s), … object(s) and 1 signature(s) uploaded.
```

Then `lower` (pull) it into the first copy:

```console
$ cd ../hello
$ forklift lower
Lowered pallet "main" to … (… object(s) and 1 signature(s) fetched).
$ cat notes.md
Signed from here on.
Edited in the copy.
```

Stop the server with Ctrl-C when you are done.

Reference: [Working with remotes](cli.md#6-working-with-remotes).

## 8. Where to go next

- [`glossary.md`](glossary.md): every forklift term, with its git equivalent.
- [`cli.md`](cli.md): every command and flag, by workflow.
- [`trust-and-identity.md`](trust-and-identity.md): users, roles, agents, and key protection.
- When a command refuses: its message names the way out, [Exit codes](cli.md#11-exit-codes) lists
  what each code means, and [`heal`](cli.md#heal--resolve-a-durability-taint-or-an-interrupted-operation)
  resolves an interrupted operation.

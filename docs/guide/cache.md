# A cache warehouse: the whole history, only the files you use

A project with years of large files, such as game assets, datasets or generated binaries, makes a
full clone heavy: every version of every file comes down, though you only ever work on the latest.
A **cache warehouse** keeps the whole history and only the files you use. It fetches every parcel
and every signature, so history, ancestry and `audit` all work offline as in a full clone. Of the
files, it fetches only those of the head you check out. Every other version stays on the server
it was cloned from, its **origin**, until a command reads it, and then it is fetched and kept.
`store evict` gives the disk back.

A cache picks *which versions* to hold. To hold only *some folders* of every version, use a sparse
clone (`clone --only`) instead; see [`clone`](cli.md#clone--clone-a-remote-warehouse).

This page follows one person on one machine, as the [tutorial](tutorial.md) does, and assumes you
have read it. As there, each `$` line is a command and the lines under it are what forklift
prints. A `…` stands for text that differs on your machine. A word in angle brackets, such as
`<first>`, stands for a value forklift prints that a later command uses: copy it from your own
output.

The test suite runs this page, as it runs the tutorial
(`crates/forklift/tests/integration/guide_pages.rs`), so the page cannot drift from the CLI. The
two sections after the last step describe refusals, which it does not run.

## 1. A project with some history

Make a small project with two versions of one of its files, and enroll in its office, so that
every parcel is signed and `audit` has something to check:

```console
$ mkdir game
$ cd game
$ forklift prepare
Prepared warehouse.
$ forklift office enroll
Enrolled "…" and established trust.
Genesis parcel:    …
Identity root key: …

From now on every parcel stacked in this warehouse must be signed. This cannot be undone (a lockout is handled by re-genesis, "forklift office regenesis").
$ echo 'Our game.' > readme.md
$ mkdir maps
$ echo 'forest, first draft' > maps/forest.map
$ echo 'castle, first draft' > maps/castle.map
$ forklift load .
$ forklift stack -m "First maps"
Stacked parcel … on pallet "main".
$ echo 'forest, redrawn' > maps/forest.map
$ forklift load maps/forest.map
$ forklift stack -m "Redraw the forest"
Stacked parcel … on pallet "main".
```

## 2. Put it on a server

A cache needs a server to fetch from. Prepare one beside the project:

```console
$ cd ..
$ forklift-server prepare --root server
Prepared warehouse at "server".
```

Open a **second terminal** in the same folder and start the server there. Leave it running:

```console
$ forklift-server serve --root server --addr 127.0.0.1:7171 --open
forklift-server listening on http://127.0.0.1:7171
```

Back in the first terminal, lift the project to it:

```console
$ cd game
$ forklift config set remote.url http://127.0.0.1:7171
$ forklift lift
Lifted the office pallet: 1 new parcel(s).
Lifted pallet "main" to …: 2 new parcel(s), … object(s) and 2 signature(s) uploaded.
```

## 3. Clone it as a cache

`clone --cache` fetches the whole history but only the head's files. The server it clones from
becomes the cache's origin:

```console
$ cd ..
$ forklift clone http://127.0.0.1:7171 game-cache --cache
Warning: FIRST CONTACT with http://127.0.0.1:7171: adopted its trust anchor, genesis …. Verify that genesis out of band. To check it before anything is fetched, pass --expect … to clone or lower.
Adopted the remote's trust anchor (genesis …); every parcel is signed from now on.
Cloned http://127.0.0.1:7171 into "game-cache": pallet "main" at … (… object(s) fetched loose).
$ cd game-cache
$ forklift remote
Remote: http://127.0.0.1:7171
Origin: http://127.0.0.1:7171 (this partial warehouse fetches what it lacks from there alone)
```

The working folder looks the same as in a full clone, and the whole history is here:

```console
$ cat maps/forest.map
forest, redrawn
$ forklift history
parcel …
author … at …
stack  … at …

    Redraw the forest

parcel <first>
author … at …
stack  … at …

    First maps
```

`audit` checks every signature, as in a full clone. It also says what the cache does not hold:
the first parcel's files, which stay on the origin, pinned by the hash that signed parcel commits:

```console
$ forklift audit
Office chain verified back to genesis ….
Pallet "main" verified: 2 signed parcel(s) valid.
This warehouse is a cache: it may lack content its history reaches. Signatures are verified in full; every tree it holds was re-hashed and every blob and chunk it names confirmed present, except those held by hash below.
1 absent object(s) are held by hash alone: each is pinned to the hash a parent that is present commits, but is not on disk, and nothing beneath it was checked.
```

## 4. Read an old version

Read the first draft of the forest. The cache does not hold it, so `show` fetches it from the
origin, checks it against its hash, and keeps it:

```console
$ forklift show <first>:maps/forest.map
forest, first draft
```

Nothing is held by hash alone any more, since that read brought the first parcel's folder in:

```console
$ forklift audit
Office chain verified back to genesis ….
Pallet "main" verified: 2 signed parcel(s) valid.
This warehouse is a cache: it may lack content its history reaches. Signatures are verified in full; every tree it holds was re-hashed and every blob and chunk it names confirmed present.
```

`diff`, `shift` and every other command that reads content do the same. A command that writes
files into the working folder, such as `shift` or `lower`, fetches everything it is about to write
in one batch first, so a file it cannot fetch stops it before it changes anything.

## 5. Work as usual

Stacking, lifting and lowering work as in any clone, and a lift from a cache uploads only the new
work:

```console
$ echo 'castle, with a moat' > maps/castle.map
$ forklift load maps/castle.map
$ forklift stack -m "Add a moat"
Stacked parcel … on pallet "main".
$ forklift lift
Lifted pallet "main" to …: 1 new parcel(s), … object(s) and 1 signature(s) uploaded.
```

## 6. Give the disk back

What a read fetched stays, so a cache grows as you look at older versions. `store evict` deletes
whatever the origin holds and nothing here needs, and records what it freed, so a later read
fetches it again. `--dry-run` shows what it would free:

```console
$ forklift store evict --dry-run
Dry run: evicting would free 6 object(s).
5 object(s) were kept because a checkout's files stand on them.
7 object(s) were kept for metadata, trust records, staged work, or a parked or in-progress operation.
$ forklift store evict
Evicted 6 object(s), freeing … B; reading one fetches it again from the origin.
5 object(s) were kept because a checkout's files stand on them.
7 object(s) were kept for metadata, trust records, staged work, or a parked or in-progress operation.
```

It keeps the files of every checkout, every parcel and signature, the office and other metadata,
and anything only this warehouse has, such as stacked work not lifted yet. It first asks the origin
which history it holds, so it never frees something the origin could not give back.

Both older versions are gone from the disk: `audit` counts the two parcels' folders as held by
hash alone, and reading the first draft fetches it again:

```console
$ forklift audit
Office chain verified back to genesis ….
Pallet "main" verified: 3 signed parcel(s) valid.
This warehouse is a cache: it may lack content its history reaches. Signatures are verified in full; every tree it holds was re-hashed and every blob and chunk it names confirmed present, except those held by hash below.
2 absent object(s) are held by hash alone: each is pinned to the hash a parent that is present commits, but is not on disk, and nothing beneath it was checked.
$ forklift show <first>:maps/forest.map
forest, first draft
```

Stop the server with Ctrl-C when you are done.

## When the origin cannot be reached

A cache can do everything offline except read what it does not hold. Run
`show <first>:maps/forest.map` again with the server stopped after an eviction, and it refuses
with exit 36 (`cache_miss`): the object "is not in this cache warehouse, and its origin could not
be reached to fetch it", and the next step it names is to run the command again once the origin
can be reached.

Run it again once the origin is back. A read such as `show` changes nothing else, and a command
that writes the working folder refuses before its first change to it; whatever either fetched
before refusing stays in the cache. `store evict` needs the
origin too, and refuses without it.

## A cache talks only to its origin

A cache never checked that any other server holds what it lacks, so it lifts to, lowers from and
fetches from its origin alone. Pointing `remote.url` anywhere else makes those commands refuse with
exit 11 (`non_origin_remote`), naming both servers. If the project moves to a new server, move the
cache with it, with `forklift remote move https://new.example.com`. It checks that the new server
holds everything this cache lacks before it changes anything. A cache also cannot be served by
`forklift-server` or mirrored to git: clone a full copy for that.

## Where to go next

- [`clone`](cli.md#clone--clone-a-remote-warehouse): the cache clone in full, including which
  commands fetch what, and the sparse clone.
- [`store evict`](cli.md#store-evict--free-disk-in-a-cache-warehouse): everything it keeps, and
  what its report counts.
- [`remote move`](cli.md#remote-move-and-remote-abort--move-a-partial-warehouse-to-another-origin):
  moving a cache, or a sparse clone, to a new server.

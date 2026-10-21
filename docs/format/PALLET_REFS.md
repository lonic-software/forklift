# Pallet refs

Pallets (git counterpart: branches) are mutable pointers to head parcels. They are the
only mutable state in a warehouse — everything else is immutable, content-addressed
objects.

## Ref files

Every pallet that has at least one parcel is a file under `.forklift/pallets/`:

```
.forklift/pallets/<name>
```

Pallet names may contain `/`, which maps to subfolders (e.g. the pallet
`feature/load-command` is stored at `.forklift/pallets/feature/load-command`). Because a
name is a file and its prefix would have to be a folder, a pallet named `a` and a pallet
named `a/b` cannot coexist — the file system enforces this, matching git's behavior.

### Name rules

A pallet name consists of `/`-separated components. Each component:

* consists of lowercase ASCII letters, digits, `.`, `_` and `-`,
* is not empty, and is not `.` or `..`,
* does not start with `-` (such a name would be indistinguishable from a flag),
* does not end with `.`,
* does not have a Windows device name as its stem (the text before its first `.`): `con`,
  `prn`, `aux`, `nul`, `com1`–`com9`, `lpt1`–`lpt9`. So `con.txt` is refused, while `console` and
  `x.con` are allowed.

These rules make every valid name mean the same ref file on every host. A ref file is the name
used verbatim as a path, so `Main` and `main` would be one file on a case-folding volume (macOS,
Windows), and Windows strips a trailing `.` and opens a device for a reserved stem. A name
outside the rules is refused, never folded to a valid spelling; the refusal suggests the
lowercase spelling when that alone would make it valid. A ref file whose name breaks the rules
(written by hand or by an older version) makes the pallet listing fail, naming the file; rename
or move it and the listing reads again. The only files skipped are `.DS_Store` and `Thumbs.db`,
which file browsers leave in folders they open, and a ref write's temp file (below). None is a
valid name, so skipping them hides no pallet.

### Content

The content of a ref file is the Blake3 hash of the head parcel in ASCII hex (64
characters), terminated by a single new line byte:

```
9028a15ad613bcd9853a3e780cfe3c78361b56ce95a2430484ba75ade5198cdc\n
```

Ref files are updated atomically: the new content is written to a temp file beside the ref,
named `<name>.tmp~<pid>-<n>` (`<pid>` the writing process, `<n>` a per-process counter), which is
then renamed onto the ref. The `~` puts every temp name outside the name rules, so a temp is never
a pallet: one left by a write that was killed before its rename is skipped by the listing, not
counted as a ref, and no pallet's ref file can share a temp's name. Only a temp whose name before
`.tmp~` is a valid pallet name is skipped; any other file spelled like a temp (`Main.tmp~1-2`)
fails the listing like any other invalid name.

## The current pallet

The file `position` in a bay's state folder (`.forklift/position` for the main tree,
`.forklift/bays/<name>/position` for a bay) records where that bay stands, as a
[state record](STATE_RECORD_FORMAT.md) of kind `position`, version 1, with two entries:

```
/forklift-position 1\n
main\n
9028a15ad613bcd9853a3e780cfe3c78361b56ce95a2430484ba75ade5198cdc\n
```

The first entry is the name of the current pallet (git counterpart: HEAD), the pallet that
`stack` advances. The second is the bay's **base**: the parcel its files and inventory stand
on, or an empty entry while the pallet is unborn. Refs are shared by every bay, so another bay
can move the current pallet past the base. Whatever reads the tree this bay holds reads the
base; whatever reads the pallet's history reads the ref. A bay whose base is not its pallet's
head is behind: commands that move the pallet refuse there, and `shift <current pallet>`
catches it up. A missing or header-less file is refused, naming the file.

A current pallet whose ref file does not exist yet is **unborn**: nothing has been
stacked on it. It is born when the first parcel is stacked. `prepare` sets the current
pallet to `main` without creating a ref file, so a freshly prepared warehouse has exactly
one unborn pallet.

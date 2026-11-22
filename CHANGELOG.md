# Changelog

Notable changes to Forklift, for people who use it. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Before 1.0, any release can change commands,
on-disk formats and the wire protocol, with no migration.

Entries are written by hand in the pull request that makes the change, under **Unreleased**. What
earns one is listed in the [contributor guide](docs/guide/contributing.md#8-keeping-docs-in-sync).
`bin/release` turns that section into the new version and refuses a release if it is empty.

## [Unreleased]

### Added

- **`forklift-server` caps how many costly requests run at once.** Ref updates, pallet deletions,
  trust installs and batch fetches, which can walk a whole history or build a bundle, now share a
  cap per serving process, set with `--max-heavy-requests` or `max_heavy_requests` (default: the
  host's available parallelism; zero is refused). A request that finds the cap full waits up to 5
  seconds and is then refused with a `503` and `Retry-After`, before it changes anything, and the
  client sends it again after the wait. A burst of pushes no longer fills the server's blocking
  threads with history walks.
- **`park` can park part of the work in progress.** `park <paths>…` parks and resets only the
  changes at or under the paths; every other file keeps its working content and its staging. An
  interrupted partial park is finished over its paths only. A path that names nothing refuses, so
  a misspelled path is never skipped silently. At `park`, a subcommand must be
  spelled out (`park a` parks `a`, never `park apply`), and a path spelled like one is `./list`.
- **Parks can be named, shown, applied, popped and dropped by name.** `park --name <name>` parks
  under a name of your choice; `park show`, `park apply`, `park pop` and `park drop` each take a
  park's name, and without one the newest park. `apply` re-applies a park and keeps it, `pop`
  re-applies it and takes it off the list, `drop` discards it without touching the working files,
  and `show` lists the files a park changes (`--json` carries the parcel, base, time and each
  file's change). Two bays popping one park no longer both apply it: the second applies nothing
  and says the park was taken.

- **`heal` rebuilds a lost or damaged metadata pallet ref.** A ref under `.forklift/meta/` (such
  as `@tags`'s) that is gone or no longer holds a parcel hash is rebuilt from its copy, or from the
  parcel its remaining bytes still name, once the head verifies as that pallet's history against
  the office. A copy that disagrees with those bytes is taken only when it descends from what
  they name; otherwise `heal` refuses with both heads (`ref_damaged`, exit 53). A ref whose bytes
  name in full a head the store has decided it does not hold, whether they read as that head or are
  damaged after it, is rebuilt at its copy's head when the
  store holds that one (the office's ref excepted), with a note naming the head it named, made before the rebuild
  is written, so it is reported whatever the write's outcome. A rebuild, of a pallet's ref or a
  metadata ref, that the pass before the taint recovery wrote but could not read back is read back
  once more right away, and is reported as rebuilt when that read finds it. Before, a pallet's ref
  was never reported, and a metadata ref was reported only in a note saying it might have been
  written. The second read comes straight after the first, so it usually meets the same fault. A failed write still unread has the
  folder sync it left unrecorded run again, and a pallet's ref still unread is named in a note
  with the head heal set out to write. A metadata ref's rebuild that pass left
  for the one after it is noted with the reason, and a refusal says each note once. A copy lost or out of step beside
  a ref that reads is set back to the ref only when the store holds the ref's head; otherwise both
  are left, with a note. A copy write that reports a failure (such as a folder sync after its rename failing)
  is now followed by a read of the copy, and a note says it holds the ref's head, or may, where
  the copy cannot be read, with what the write reported; before, the pass before the taint
  recovery said nothing of it and the pass after read the copy as in step. Where no taint records
  the folder it left unsynced, `heal` syncs that folder again, also for a copy it cannot read
  back, and says what that sync reported; when it does not hold either, `heal` says in place of
  the note that the copy may not be durable, and goes no further. Every `heal` now first syncs
  every folder of the pallet refs and the folders of the metadata refs and their copies, so running
  `heal` again syncs a rebuild or copy write left that way again, as the note now says; before, the
  note said a later `heal` would not sync it again. A folder that cannot be synced stops `heal`
  before it changes anything, naming the folder. The office's ref is rebuilt first, at a
  head that verifies against the anchor of the trust file or of its copy, and a trust file lost
  beside it is then restored from its copy. The redactions' own ref (`@redactions`) is rebuilt
  next, ahead of every other metadata ref, which is checked against it; beside a damaged office
  the head it is to be rebuilt at is verified first, and the office is checked against that head.
  While either ref is damaged and was not rebuilt, every damaged metadata ref is refused, naming
  that ref's step. A metadata ref that cannot be settled before the taint recovery is judged again after
  it; a refusal that stops the run before then, such as one for a durability taint that still
  stands, names each ref left that way with the heads found for it, and each damaged metadata
  ref a pass that stopped never judged. A ref left that way that cannot be read keeps the
  recovery from checking what it reaches, so the taint can stand on it for good: that refusal
  says the ref is judged again only once the taint is resolved, and names a backup or a fresh
  clone as the way out for when it is not. A damaged user pallet ref whose unread head keeps the
  taint standing now gets that way out too, where the refusal said `heal` rebuilds it once the
  taint is resolved. Inside `heal`, the note on a ref that could not be read no longer tells you
  to run `heal`; any other step the read names, such as restoring the ref from a backup, is kept.
  `heal --remove-ref <pallet>`, refused because something still names a head for
  the ref, names `heal --ref` only where no durability taint stands. While one does, neither
  that write nor the removal's own can complete, so the removal is refused naming `heal` first, to
  resolve the taint, whether or not something names a head.
  Where damaged tombstones would refuse a pallet ref's rebuild,
  `heal` now writes no user pallet ref and stops the run, with their repair as the step; the refusal
  names each ref it would have rebuilt and the head, each other it refused, and each damaged pallet
  ref the run stopped before judging. Before, it stopped at the first such ref and left the rest
  unnamed.

- **`heal` names its way out for every damaged metadata ref it does not rebuild, and never
  removes one.** Where `heal` has no head it can rebuild the ref at (nothing names one, or what does
  is several parcels, one possibly lost, a copy and bytes that diverge, or a head whose history does
  not verify), the step is to restore the ref and its copy from a backup. Where its damaged bytes
  name in full a parcel the store has decided is gone and `heal` does not rebuild it at its copy
  (it has none, the copy names no head or that parcel too, or it is the office's ref, which is never
  taken back to an older head this way), `heal` names that parcel, and the step is to restore it from a backup
  or a fresh clone, or the ref and its copy from a backup. `heal --remove-ref
  @<name>` is refused before anything changes. Where the refusal is no verdict on the ref, the step
  is the one `heal` names: for a ref that waits on the office's or `@redactions`' refusal, or on an
  unborn office, that step; where `heal` could not read what it needs (a file it may not read, pack
  damage it rebuilds, a pack it cannot list in full), that read's own step, else settling it; where
  it could not write the rebuild it chose or read it back, the write's own step, else settling why;
  where what it needs is in a format this Forklift does not know (an object, a record or a signature
  a newer version wrote), upgrading; and where the trust state refuses, `heal` again when it restores
  the trust file from its copy, else a way out of that refusal (a lost anchor's, or one the trust
  file's own refusal names) or settling why the office does not verify. A parcel of its history
  that carries no signature after trust, there or on the office pallet, refuses as
  `signature_missing` (exit 71), naming a backup or a fresh clone. A command that reads a damaged
  metadata ref names `forklift heal`; where `heal` would not judge the ref (its copy cannot be
  read, or something that is not a file stands at the ref's or the copy's path), the step that lets
  it; and where neither its copy nor its own bytes name a head (the copy holds no parcel hash and
  the bytes hold neither a hash nor a prefix of one), restoring the ref and its copy from a backup:
  `heal` has nothing to rebuild it from, so that is its way out too, once nothing else it waits on
  stands.

### Changed

- **While a durability taint stands, a refused write leaves the file as it was.** A write of a
  single file on its own, such as a pallet's ref, that a standing taint refuses is now refused before it
  replaces anything, where before it was written and then reported as refused. `heal`'s own
  writes are not refused by the taint it is resolving, so `heal --ref <pallet>=<parcel>` now sets
  the ref and goes on to resolve the taint, where before it was refused until a plain `heal` had
  run.

- **`heal` no longer rebuilds a pallet's ref in place of the parcel its own damaged bytes name.**
  A damaged ref whose bytes name in full a parcel the store has decided is gone, with no
  durability taint the pass read covering it, is first looked up one character off: a parcel the
  store holds at one changed character of that hash, or with one inserted character taken out, is
  what the damage most likely made it from, and is taken as what the bytes name, as any salvage
  match is. This lookup applies to metadata refs too. Where none matches, a pallet's ref is
  refused with `ref_damaged` (exit 53), naming the parcel and its restore from a backup or a
  remote, with every head found listed for `--ref`. Before, those bytes counted as naming nothing, so `heal` rebuilt the ref at
  whatever head a checkout, journal or interrupted operation named, with exit 0 and no word of
  the parcel, or refused saying nothing names its head. `--remove-ref` still removes such a ref
  where nothing stands on its pallet, keeping its bytes; `--ref` notes the parcel, served or not,
  since setting the ref replaces its bytes without keeping them (and, served, the head cannot be
  checked against it). Under a refusal whose list of heads is empty, the line now reads "No
  candidate head was found for it." in place of "Nothing names a head for it.", which
  contradicted a reason naming a parcel.
- **Each metadata pallet's ref now has a copy.** Every change to a ref under `.forklift/meta/` also
  writes the same bytes to `.forklift/meta-copy/<name>`, under the same lock, in the order that
  leaves the copy at the newer head if the write is interrupted. Beside a ref that reads, garbage
  collection keeps the head the copy names as well as the ref's own.
- **A lost metadata pallet ref is refused where it is read, not read as empty.** A ref under
  `.forklift/meta/` whose file is missing while its copy stands now refuses as `ref_damaged`
  (exit 53) in every command that reads it, listings included, where it read as a pallet never
  written: `tag list` printed no tags, `lower` took the remote's head as a first adoption, and
  garbage collection held nothing for it. The next step names `forklift heal`, which judges the ref
  (or, where the copy holds no parcel hash, restoring both from a backup, since `heal` has nothing
  to rebuild it from); a
  copy that cannot be read, or a link at its path, is refused, naming what to make readable or replace.
  A link at any pallet's ref path is never followed: it refuses as blocked, naming moving it aside,
  and `heal` leaves it with the same step. A ref whose path cannot be read refuses, naming making
  the ref's file and the folders on the way to it readable, where it read as a pallet never written. A ref lost together with its copy still reads as empty. `heal`'s `objects_lost`
  report names each pallet whose ref could not be read (`pallets_unread` in `--json`) instead of
  leaving it out, and no longer says no pallet's history reaches the objects while one is named (`--json`
  schema 60).
- **A killed process no longer leaves a lock to remove by hand.** The bay's lock, the object
  store's lock and a served warehouse's `serve.lock` are now kernel locks, released when the
  process holding them ends, however it ends. A `forklift-server` that crashed or was killed
  restarts under a supervisor with no manual step, and `gc` runs after it; a `forklift` command
  killed mid-way leaves the next one free to run. A lock another process still holds is refused at
  once, coded `warehouse_locked`, `store_locked` (new) or `serve_locked` (new), all exit 6, with a
  next step that says to wait for it or stop the server. JSON schema 59.

- **A lost or damaged trust file refuses as `trust_damaged` (exit 69).** A trust file missing under
  a born office was read by `stack` and `deliver` as a warehouse that never enrolled, and one that
  did not parse refused uncoded. Now every command that reads the trust state, stack, deliver, git
  import, enroll, regenesis and every command that judges a remote's trust, refuses either as
  `trust_damaged`, before writing anything, and names the ways out (`heal` refuses as `trust_damaged`
  where its repair needs the remote, and elsewhere reports a failed handshake and goes on with its
  other repairs). A remote whose own trust file is lost answers `trust_damaged` too, with its
  operator as the next step. `lower` alone reads a missing
  trust file as no anchor held, so it can adopt the anchor of a remote you trust. A server answers
  it as a `503`.
- **`heal` restores a lost or damaged trust file from its copy.** Before anything reads the trust
  anchor, and whether or not a taint stands, `heal` writes the trust file back from its copy when
  the trust file is missing, does not parse or no longer verifies the office pallet and the copy
  does; and it writes the copy from the trust file the other way round. Such a warehouse used to
  refuse every signing write, and `heal` itself, as `trust_damaged`. A served warehouse's trust
  change that stopped before its copy was written is finished, as is its first anchor written before
  its office pallet was lifted. Where the two differ and both would serve, `heal` refuses as
  `trust_damaged` and changes nothing: move aside the one you do not trust, then run it again. It
  reports what it wrote as `trust_restored` (`--json` schema 57).
- **Trust and signature refusals say when `heal` mends them.** A `trust_damaged` refusal now names
  `forklift heal` as its next step wherever heal can restore the trust file from its copy, a
  server's `503` included; a trust file that parses but no longer verifies the office pallet while
  its copy does is refused that way too, rather than with the office's uncoded message. On a served
  warehouse the ways out no longer name `forklift lower`, which it refuses. A lost office pallet ref
  beside a trust file refuses as `ref_damaged` (exit 53), naming `forklift lower` as its next step where
  `lift` finds the remote holds this warehouse's office, and as a way out where a remote is configured; `office enroll` no longer calls such a warehouse already enrolled. A parcel's stored
  signature that cannot be read refuses as the new `signature_damaged` (exit 70), and a parcel
  stacked after trust was established with no signature as `signature_missing` (exit 71), where
  both were uncoded (`--json` schema 58); a server answers damage to a signature in its own office
  pallet as a `503` `signature_damaged` for its operator.
- **A served warehouse's trust changes only through its server.** In a served folder,
  `office regenesis`, `office enroll` and a `lower` that would move the office pallet now refuse,
  leaving the trust file and the office pallet as they were; run them in a clone of the warehouse
  instead. Every trust change now writes a
  copy of the trust file (`.forklift/trust-copy`) last, and a change run again after it stopped
  writes the missing copy. A server's first serve refuses a warehouse whose trust file does not
  verify its office pallet, whose copy differs from it, or whose trust file is missing beside a born
  office pallet or a standing copy, as `trust_damaged`, and one whose trust file stands beside an
  unborn office pallet with no matching copy, or not reading as an anchor (an enrollment or first `lower` that stopped part way)
  as `ref_damaged`; before, such a warehouse was served as it stood. A trust change refuses before
  it writes anything when the copy's path holds something other than a file, or a copy a newer
  Forklift wrote.
- **Every local state record carries a digest.** The records of a bay, its pallet position and
  scope, a merge, cherry-pick or park in progress, the inventory metadata, the redaction and
  origin records, the clone-in-progress record and the taint files end in a digest of their
  bytes, as the server's records do. Damage that still parsed, such as a flipped digit in a hash
  or a cut on a line boundary, was read as a different or shorter record: a cut scope file read
  as the full scope. Such a record now refuses as damaged. A full scope is no longer the empty
  file, so an empty scope file refuses too. **Breaking:** these records written before this
  change refuse as an older version's, so clone or make the warehouse again. The undo journal
  moves to version 5 as well, and an older one is refused, so undo history from before the
  upgrade is lost.
- **A server's withheld record, tombstone list and served mark carry a digest.** Damage that still
  parsed, such as a flipped digit in a hash, was read as true: a takedown was served again under
  another hash, or a deleted pallet's history was left for gc. A record that does not match its
  digest now refuses with `withheld_damaged` or `tombstones_damaged`, like any other damage.
  **Breaking:** records written before this change have no digest and refuse the same way, so run
  `forklift-server tombstone repair` and then `forklift-server withhold repair` on a warehouse
  served before it, in that order: the withheld repair reads the tombstones. When the old record
  failed its digest, each repair says that an entry it kept may still have been altered. `withhold
  repair --header-damaged` is gone: a header damaged into a newer version's no longer matches its
  digest, so it is repaired like any other damage.
- **A repaired withheld record no longer lets a lost takedown be served.** `forklift-server withhold
  repair` records each absent file it cannot account for, and each absent folder or part of history
  it cannot walk beneath, and the warehouse is not served while one is outstanding: `serve --root`
  will not start, and a `--warehouses` server answers `409`. Before, a takedown whose entry was lost
  was served again unless its operator withheld it again by hand. Put the bytes back and run the repair again, withhold it
  with `withhold add`, or, once nothing kept names it, run the repair again; `withhold list` names
  each. **Breaking:** the withheld record moves to version 4, so a record written before this change
  refuses as an older version's; run `forklift-server withhold repair` on it.
- **A served warehouse whose host records do not parse refuses with a code, and is not retried.**
  A `--warehouses` server answered an uncoded `500` for a root whose sealed-object, unavailable or
  origin record did not read, or whose fetch scope stood in a full warehouse. Each now answers
  `503` with no `Retry-After` and its own code: `sealed_damaged` (exit 59), `unavailable_damaged`
  (exit 60), `origin_damaged` (exit 61) or `fetch_scope_stray` (exit 62). The message names no
  server path, and the next step is `forklift heal` where heal rebuilds the record. A client names
  the remote's operator and does not retry. The same codes refuse locally, where a damaged origin
  names `forklift remote move <url>`, and a partial warehouse is refused as partial whatever its
  fetch scope holds.

- **Parked work is shared by every bay, named, and rebuilt by `heal` when its record is damaged.**
  Each bay kept its parked work in its own `parked` file, read as empty when it was lost, so the
  parked work was gone without a word, and gc could free its content. Parked work now lives in one
  record, `.forklift/parked`, which every bay shares and which carries a digest. A missing or
  damaged record refuses `park`, `park list`, `park pop`, gc and every other reader, naming
  `forklift heal`. `heal` rebuilds it from the entries that still read, any bay's leftover
  `parked` file, and the parked parcels the store holds, keeping a copy of the damaged one
  (`parked_rebuilt` in `--json`). Each park has a name, by default the pallet and the time
  (`park list` prints it, and `--json` carries `name` and `recovered`, set on a park the store
  scan brought back). Warehouses made before this have no record and refuse until `heal` runs.

- **`heal` repairs a lost or damaged record of applied redactions.** A damaged record of which
  redactions this warehouse applied (`.forklift/redactions-applied`) failed every read of redacted
  content, so `git export` failed, with no remedy named; a lost one listed each redaction as pending
  over content already deleted, and `heal` refused with exit 58 (`objects_lost`). Now `heal`
  rebuilds a damaged record from the hashes that still read and that a redaction names, keeping a
  copy of it (`redactions_applied_rebuilt` in `--json`), and applies again each redaction whose
  content is gone, as `redact apply` would (`redactions_reapplied`), so exports leave the file out
  again. A damaged record's error now names `forklift heal`.

- **`heal` repairs a lost or damaged record of content the remote withholds.** When the record of
  what a fetch left out because the remote's host withholds it (`.forklift/unavailable`) was lost,
  `heal` refused with exit 58 (`objects_lost`) and `git export` failed on the missing content; a
  damaged one also stopped every read of it, with no remedy named. Now `heal` rebuilds a damaged
  record from the entries that still read, keeping a copy of it (`unavailable_rebuilt` in `--json`),
  and records again each absent file the remote's host still withholds (`recorded_unavailable`),
  so checkouts and exports leave it out as after the fetch. A damaged record's error now names
  `forklift heal`.

- **`heal` repairs a cache whose sealed record was lost, and a lost origin record names a move that
  works.** A cache that lost entries of its record of content held by hash read that content as
  lost, and `heal` refused with exit 58 (`objects_lost`), though the origin still held it. Now `heal`
  asks the origin which history it holds and records again each lost absent object that history
  reaches, listed in `--json` as `resealed`; a read then fetches it as before. An origin that does
  not answer leaves them lost and names `forklift heal`. With a remote configured, a missing or
  damaged origin record of a partial warehouse is refused with exit 11 (`non_origin_remote`), `heal`
  included, where a damaged one failed with no code, and its next step names `forklift remote move`
  with that remote instead of a `<url>` placeholder. Bare `remote` reports it as a block.

- **A read the warehouse cannot serve names a remedy that works.** `store peek <hash>` of a hash
  the store does not hold now says only that, with no next step, where it was refused with exit 57
  (`object_unreadable`) naming `heal`, which then found nothing to heal. An absence that still leaves
  something undecided, such as a pack with no index, keeps exit 57. In a sparse clone, `show` and
  `redact record` of a path outside the fetch scope are refused with exit 7 (`out_of_scope`),
  naming `forklift scope fetch <directory>` for a directory at the base, or a fresh clone when the
  path is a file at the root.

- **A corrupt loose object is refused with a code, and `heal` names it as lost.** A loose object
  file whose bytes did not decompress, or decompressed to other content, failed reads with an
  uncoded message, and `heal` exited 0 over it, since its closing check never read a file's bytes.
  Now such a read is refused with exit 57 (`object_unreadable`), and `heal` reads every loose file:
  one that history still reaches is counted as `corrupt` in its `objects_lost` refusal (exit 58),
  with the pallets that reach it. The file is left in place; one no history needs, or one at an
  address the warehouse holds by hash alone, redacted or withheld, is noted, not counted lost. A
  loose file `heal` cannot read at all is not judged: it says its check may be short. The `--json`
  schema is now 47.

- **A damaged pack record is refused with a code, and `heal` names it as lost.** An object whose
  record in a pack did not decompress or rebuilt to other content, or a delta standing on such an
  object, failed reads with an uncoded message; `heal` exited 0 over a damaged file record, and
  stopped with an error, naming nothing, over a damaged directory record. Now such a read is refused
  with exit 57 (`object_unreadable`), and `heal` decodes every pack record: one that history still
  reaches and that no other copy stands in for is counted as `corrupt` in its `objects_lost`
  refusal (exit 58). The pack is left as it is; a damaged record no history needs, or one the
  warehouse holds by hash alone, is noted, not counted lost. This makes `heal` read the whole pack
  store.

- **A damaged pack file is refused with a code, `heal` rebuilds a lost or damaged index, and `heal`
  says when objects are lost.** A read that met a lost, cut-short or corrupt pack index, a missing
  pack data file or a damaged index entry failed with an uncoded message, or reported the object as
  simply missing. Now it is refused with exit 57 (`object_unreadable`), naming `forklift heal`.
  `heal` rebuilds a lost or damaged index from its pack's data file, keeping a replaced index beside
  it, and the refused command then works. When objects that history still reaches cannot be read
  after every repair, `heal` refuses with exit 58 (`objects_lost`), counting them, listing up to 20
  hashes and naming the pallets that reach them, instead of reporting success. Remotes answer
  `object_unreadable` as a 503 naming their operator's `forklift heal`, which clients do not retry.
  The `--json` schema is now 46.

- **A served warehouse's lost or damaged record of withheld content is refused, and its operator can
  repair it.** The record of legal takedowns on a server, `.forklift/withheld`, says which content it
  must never serve. A record that did not read made every handshake fail with an uncoded HTTP 500
  naming the server's path. A deleted record was read as nothing withheld, so clones failed on
  content the server had already deleted, and content it still stored was served again. Now a served
  warehouse's first serve writes an empty record, so a record lost later is damage. Every request
  that reads it is refused with exit 56 (`withheld_damaged`), naming
  `forklift-server withhold repair --root <warehouse>`. The server answers 503 with that code and no
  `Retry-After`, so clients stop and name the remote's operator instead of retrying. The repair runs
  while the warehouse is not served:
  - It keeps a copy of the damaged record beside it and keeps every entry that still reads.
  - It keeps a hash whose entry lost its statement withheld under a placeholder statement, to
    restate with `withhold add`. A line cut short or not valid UTF-8 counts as having lost it.
  - It lists files the history names that are absent and that no record holds: what a lost entry
    withheld once its copy was deleted, for the operator to withhold again.
  - A record that matches its digest but whose header names a newer version is left for that
    Forklift.

  A lost entry whose content is still stored cannot be found, and the report says so. Warehouses
  served before this change have no record, so they have to be repaired before they are served.

- **A served warehouse's lost or damaged tombstone list is refused, and its operator can repair it.**
  The list of pallets deleted on a server, `.forklift/tombstones`, keeps their heads from gc. A list
  that did not read made every handshake fail with an uncoded HTTP 500. A deleted list was read as
  empty, so gc or a full compaction could free history that clones had pruned against. Now a served
  warehouse whose list or `served` mark is lost or damaged refuses whatever frees history: gc, a full
  compaction, scope prune. These refuse with exit 55 (`tombstones_damaged`), naming
  `forklift-server tombstone repair --root <warehouse>`. The server answers 503 with that code and no
  `Retry-After`, so clients stop and name the remote's operator instead of retrying. `heal`, which
  must not rebuild a ref over a list that may record its deletion, refuses the same way. The repair
  runs while the warehouse is not served:
  - It moves the damaged files aside and keeps every entry that still reads.
  - It gives a lost deletion its name back when the bays' undo journals settle it.
  - It keeps every other piece of history no pallet, office pin or tombstone holds, with no name,
    through gc and compaction. `tombstone list` shows these as `(kept, no name)`, and
    `tombstone drop --head <hash>` releases one.

  The list format is now version 2, which can hold a kept head. Warehouses served before this change
  have to be repaired, or made again, before they are served.

- **A lost or damaged fetch scope is rebuilt by `forklift heal`.** In a sparse warehouse, a
  `.forklift/config/fetch-scope` that was deleted, emptied, zeroed or no longer valid UTF-8 made
  every command refuse with "clone the remote again", while `heal` reported nothing to heal. The
  refusal now names `forklift heal`, which rebuilds the fetch scope from the scopes of the warehouse's
  checkouts once the history there is all present. When it cannot (an object is lost under them, no
  checkout's scope can be read, or a readable fetch scope does not hold a checkout's scope), `heal`
  changes nothing and refuses with exit 54 (`fetch_scope_damaged`), naming a next step:
  `heal --fetch-scope <path>`, once per path, sets the fetch scope to the paths given, fetching from the origin
  whatever the history at them lacks.

- **A lost or damaged pallet ref is rebuilt by `forklift heal`.** A ref file that was deleted,
  truncated, zeroed or had a byte changed, or a `pallets` folder that was removed, used to leave the
  warehouse unable to stack: `stack` refused as behind and named a `shift` that refused too, or
  failed with "fix it by hand", while `heal` reported nothing to heal. Every command that meets such
  a ref now refuses with exit 53 (`ref_damaged`) and names `forklift heal`, which rebuilds it at the
  one head the warehouse's own evidence agrees on: the bases checkouts stand on, their journals, an
  interrupted operation, and what the damaged file's bytes still name. When the evidence diverges,
  names nothing, or records the pallet as deleted, `heal` changes nothing, exits 53 and lists every
  head it found; `heal --ref <pallet>=<parcel>` then sets the ref, and `heal --remove-ref <pallet>`
  drops an unreadable ref that nothing names a head for, keeping its bytes under
  `.forklift/removed-refs`. A lost ref that no checkout stands on is not found.

- **Losing the warehouse marker no longer loses the warehouse.** `.forklift/warehouse` now ends
  in a digest of its own bytes and is kept twice, beside `.forklift/warehouse-copy`, each written
  in turn. A marker that is deleted, truncated or damaged is read from its copy and rewritten at
  the next command, where it used to make the folder "not a warehouse" (exit 3) with a way out
  that set the store aside, or, when a changed byte still parsed, was read silently. Only when
  both are missing is the folder not a warehouse. A file that cannot be read is also read through
  its copy, but left as it is. With no whole copy, a damaged marker is refused as damaged and one
  that cannot be read is refused with its read error. A warehouse made by an earlier unreleased
  build, whose marker carries no digest, is refused and must be made again.

- **A query predicate that could be read two ways is refused, never widened.** `forklift query
  --where`, and the MCP server's `query` tool, used to read a JSON object that named a key twice
  by its last spelling, so `{"all": [<filter>], "all": []}` matched every parcel. An empty
  `matches` pattern or `--touches` path also matched everything, and a `--signer` or `parcel`
  prefix that was empty matched every hash, while one in capitals matched none. All of these are
  now refused with exit 18 (`query_predicate_invalid`). The MCP server answers any message naming
  a key twice with `invalid_request`. A `--signer` typed in capitals is lower-cased first, and
  flags given beside `--where` no longer count against its depth limit.

- **Every object has one spelling.** An object is named by the hash of its bytes, but its header
  could carry padding or trailing bytes, a number could be written with redundant bytes, and a tree
  could list its entries in another order, so one file, tree or parcel could be stored under more
  than one hash, out of reach of a signature or a redaction that names the other. Forklift now
  refuses, when reading and when storing, any object that is not exactly the bytes its format
  writes; a refused object reads as malformed. Objects written by Forklift are unaffected.

- **A refused clone bundle installs nothing, and each pack record has one spelling.** A bundle
  whose signature section was refused used to leave its packs installed. The client now reads and
  checks the whole bundle, signatures included, before it installs any pack from it. It also
  refuses a pack record that does not hold exactly one zstd frame, and a pack index whose fanout
  width differs from the one Forklift writes. Bundles built by Forklift are unaffected.

- **Every signed meta record has one spelling.** A meta-pallet cell (`@kv`, `@manifest`, `@tags`,
  `@haul`, `@redactions`) and an office user or key record used to be accepted in any TOML spelling
  of its data: with comments, blank lines, other quoting, escapes or number bases, keys in another
  order, and, for office records, fields Forklift does not write. One signed record could
  therefore be stored as several byte strings. Forklift now writes these records in one documented
  spelling and refuses any other, when reading and when a parcel would add one. Hex in a key or
  signature is accepted in lower case only. A cell value can no longer hold a float or a date-time.
  Earlier builds wrote most cells differently (keys in insertion order, other quoting), so a meta
  pallet written before this change is refused when it is fetched or verified, and has to be
  written again.

- **A chunk-list redaction no longer stops a lift or a clone over withheld content.** Where the
  `@redactions` pallet holds a redaction of a chunked file, forklift-server used to refuse a lift
  (`500`), and a full `clone` or `lower` to fail, as soon as a head named a chunked file the host
  withholds and no longer holds, since the file's chunk list could no longer be read. The host now
  records that chunk list's digest while it still holds the file, or before it deletes it, and
  reports it to clients in the warehouse's `withheld` list; both judge the file by it. A full
  warehouse with no remote that cannot compare a withheld file this way leaves the redaction
  undecided rather than calling it applied. The AWS head records and reports the digest as well,
  on its `withheld#<hash>` item. The `withheld` and `unavailable` records are now format 2: a
  record written by an earlier build is refused until it is removed.

- **A lift gives the host the chunk list of a withheld file it never held, on either head.** When
  the host withholds a chunked file before it ever held a copy, a lift from a warehouse that holds
  the file now sends the host the file's recipe (the list of its chunks, never the content) so it
  can record the chunk-list digest, and the host keeps nothing of it. The handshake advertises this
  with `withheld_intake`, which names the way the head takes it. A recipe that could not be sent
  leaves the lift's result as it was and writes a `withheld_intake_failed` warning.

- **forklift-server refuses a lift naming a withheld file it cannot judge.** When the host
  withholds a chunked file whose chunk list it never learnt, a lift whose new history gives that
  file a chunked entry while the pallet's head names it as a chunked file nowhere (a new pallet has
  no head, so any of its history counts) is now refused with `content_withheld`, naming the file,
  instead of landing a head the server could never judge against a chunk-list redaction. The way
  out is to lift from a warehouse that holds the file, which sends the server its recipe first. A
  lift onto a pallet whose head already names the file as a chunked file, moving it included,
  still lands. The AWS head refuses it the same way.

- **Commands wait for a remote that is unavailable for a moment.** A remote that answers `503`
  with `Retry-After`, as a server does while it deletes or settles redacted content, or while the
  AWS head deletes a redacted file's chunks, is asked again after the wait, for up to 2 minutes,
  instead of failing `clone`, `lower`, `lift` and every other command that talks to it at once.
  A `lift` also waits out a rate limit on its uploads and on moving the remote's pallet. A `503`
  without `Retry-After` still fails at once.

- **The AWS head keeps each warehouse's objects apart.** Objects, signatures and staged uploads are
  stored under the warehouse's own keys in the bucket, so one warehouse can no longer learn whether
  another holds an object. This is AWS format 2: a warehouse made by an earlier build is refused
  with a message saying to make it again under a new id, and `forklift-aws-upgrade` refuses it the
  same way.
- **`git export` says what it leaves behind.** With a remote configured, it asks the remote for its
  pallets and warns about each one it has no branch for as it stands there, naming the `lower` that
  brings it here, and about tags the remote has and this warehouse does not; leaving from a fresh
  clone no longer silently drops every other pallet. Its last line now names each Forklift-only
  record it drops, `@haul` and `@kv` included, and a warning counts parked work. In `--json`:
  `not_exported`, `parked`, `remote_left_behind` and `remote_unchecked`. A new guide page,
  `leaving.md`, walks through moving a project back to git.
- **Tags, the manifest, keyed values and hauls work without enrolling.** In a warehouse that
  never ran `office enroll`, `tag create`, `manifest note/approve/provenance/set/unset` and every
  `haul` command now record unsigned entries naming your configured operator, instead of
  refusing. There are no roles until you enroll, so anyone may tag or record a merge. Readers
  mark these entries `(unsigned)`, and in `--json` the `signer` field is absent (hauls carry
  `"unsigned": true`). Once a warehouse has an office, every entry is signed as before.
- **`deliver` works without enrolling.** In a warehouse that never enrolled, the delivered parcel
  and its delivery entry are recorded unsigned under your configured operator, instead of the
  delivery being refused. Once a warehouse has an office, both are signed as before; one whose
  trust anchor was lost refuses, saying so.
- **`git export` writes unsigned tags.** A tag recorded without a key becomes an annotated git tag
  with its declared operator as tagger, ending in a `Forklift-Tag-Unsigned: export/1` line instead
  of a signer key, so a warehouse with no office exports its tags. An office that cannot be read
  now leaves out only the signed tags, each named in a warning.
- **`office enroll` keeps metadata recorded before it.** Unsigned tags, manifest entries, keyed
  values and hauls are folded into the trust boundary and stay valid; everything written after
  enrolling is signed. A server's first enroll does the same. A `lower` that adopts a remote's
  anchor names any local metadata the anchor does not vouch for in a `meta_unvouched` warning.
- **`lift` re-signs your metadata from before trust.** Entries you recorded unsigned before the
  remote enrolled are replayed under your key as the lift sends them, and listed as re-signed;
  another operator's are refused by path, and `lower --keep @<pallet>:<path>=theirs --confirm`
  drops them, even from a pallet the remote does not hold yet. Without a key, the refusal says
  how to get one admitted.
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

- **The host of an AWS warehouse can withhold content too.** `forklift-aws-withhold --table …
  WAREHOUSE add HASH --statement "…"`, run under the operator role, withholds a file's content
  there, and `list` shows what is withheld and how far each has got. From the next request on, the
  AWS head answers a read or upload of it `451 content_withheld` with the statement, leaves it out
  of batches, never asks a lift for it and answers it in the upload negotiation's `withheld` list,
  lists it in the handshake, refuses a lift commit naming it, and accepts history naming it
  without the content. Its stored copies are deleted as a redacted file's are: staged uploads at
  once, and about half an hour later the stored file and any batch response that may hold it,
  unless the history of a meta pallet or of the trust anchor's adopted history names it, in which
  case that one copy is kept and the handshake keeps reporting its deletion as owed. Unless that
  copy is kept, an update to a meta pallet that would newly name it is refused `451`. A chunked file's chunks
  that only it names are deleted, and then its recipe, as for a redacted file.

- **A cache or sparse warehouse keeps content its remote withholds.** `store evict` and
  `scope prune` no longer free a copy of content the remote's host withholds, as for a legal
  takedown, since the remote may not serve it again; the folders above it stay with it, and both
  reports count what was kept in `kept_withheld`. `--json` schema is now 34.

- **A lift leaves out content its remote withholds, and says so.** A lift no longer offers
  content the remote's host withholds, nor the chunks of a withheld chunked file (a chunk another
  file shares is still sent, unless the host withholds that chunk too), since the remote takes
  the history without them. The result lists what was left out, with the host's statement, as
  `withheld`. `--json` schema is now 35.

- **A lift uploads again when the remote deletes content under it.** A server or the AWS head
  that answers content present and then deletes it, as content it owed a deletion, before the lift
  moves the pallet now refuses the move with `remote_history_incomplete` (exit 52) instead of an
  uncoded error. A lift refused that way asks again what the remote lacks, uploads it, and tries
  the move once more; a second refusal fails the lift with that code. `--json` schema is now 36.

- **A warehouse records content its remote withholds and it lacks.** The new `unavailable` record
  lists such content beside the host's statement. An absence it lists is not damage: `audit` counts
  it apart, as `unavailable` in `--json`, and passes, `heal` reports it apart, and nothing records
  it as held by its origin. forklift-server refuses to serve a root lacking content the record
  lists. The record drops an entry once the content is back, or once nothing names it.
  `--json` schema is now 37.

- **A file whose content the remote's host withholds stays in the head, not checked out.** Where the
  `unavailable` record lists a file's content and this warehouse lacks it, a head move such as
  `shift` or `undo`, and `park`, `park pop`, `unload` and a directory `restore`, write no file at
  its path and keep it staged as the head names it, so the next parcel still names it; a load never
  turns its absence into a removal, and `forklift remove` stages one on purpose. A file put at the
  path is loaded as a change. `stocktake` lists such files apart, as `unavailable` in `--json`, and
  they do not make the bay unclean; `audit` and `heal` do not count staged ones as lost. `--json`
  schema is now 38.

- **`clone`, `lower` and `scope fetch` take a head naming withheld content.** Content the remote's
  host withholds, as for a legal takedown, is no longer asked for: a full, `--cache` or `--only`
  clone, a `lower` and a `scope fetch` take the history without it, record it in the `unavailable`
  record with the host's statement, and check the head out without the file, instead of failing
  with `content_withheld`. A cache never fetches it when a head move needs the file, and `heal`
  finishes an interrupted `lower` without it, removing the file's old version. A `lift` that
  auto-merges a diverged remote head takes that history the same way rather than failing with
  `content_withheld`; a merge that would have to read the file refuses with `content_withheld`
  before it changes any file.
- **Reading a file whose content is withheld says so.** `show`, `blame`, a `diff` that prints the
  file's lines and a `restore` of it, for a file this warehouse never received because its remote's
  host withholds the content, now fail with `content_withheld` (exit 51) and the host's statement
  instead of a missing-object error, and so does a `lift` of such history to another remote. A
  merge that must read such a file now says the way out: take your own change to it out, and the
  merge leaves the file out. `--json` schema is now 39.
- **`git export` and `git mirror` leave out a file whose content is withheld.** A file this
  warehouse never received because its remote's host withholds the content is left out of each
  commit that names it, with nothing in its place, instead of failing the export or the mirror with
  `content_withheld`. Both name it with the host's statement and the parcels whose commits left it
  out (`withheld` in `--json`, beside `redacted`), and `git mirror` takes the remote's new history
  without the content, as `lower` does. `--json` schema is now 40.
- **`heal` says when what it lost is withheld.** `heal` no longer asks its remote for an object
  whose content the remote's host withholds: it names the object with the host's statement and
  says a re-run will not bring it back, instead of telling you to re-run once connectivity is
  restored, and it still fetches back the damaged objects beside it. Its fetch of newer history
  naming withheld content no longer fails, and it never records that content as unavailable.
  `office accept-regenesis` fetches boundary history naming withheld content without it, as
  `lower` does.
- **`consolidate` and `pick` take a file whose content is withheld.** A merge or pick that takes
  the other side's version of a file whose content this warehouse does not hold because the
  remote's host withholds it no longer fails reading it: the file is left out, its old version is
  removed, and it is staged as not checked out. A merge that would have to read that content, to
  merge both sides' changes or to keep a version in conflict, refuses with `content_withheld`
  before changing any file. `abort` and `heal` put back the file's old version.

- **`remote move` knows what the new origin withholds.** A cache or sparse warehouse moving to a
  server whose host withholds content it relies on its origin for no longer counts that content as
  held there. The move asks the previous origin: while that origin answers and does not withhold
  the content, the move is refused with `origin_move_incomplete`, naming it in `details.withheld`; when it withholds
  it too, is not recorded or does not answer, the move goes ahead and lists the content in
  `withheld`, with why in `previous_origin_withheld`. `--json` schema is now 33.
- **A forklift-server host can withhold content, as for a legal takedown.** `forklift-server
  withhold add --root … HASH --statement "…"` stops the server serving or taking a file's content
  at once: a read or upload of it is answered `451` with the host's statement; `withhold list`
  shows what is withheld. History is unchanged and pallets keep naming the file, and a lift may
  keep naming it too without uploading it: the upload negotiation answers it in a new `withheld`
  list. Within seconds the running server cuts a bundle download already under way, deletes its
  stored copy unless metadata history, the history the trust anchor adopted, or a checkout's
  unfinished work still names it, and rebuilds the bundle when `--rebuild-after-lifts` is set; one
  withheld while nothing serves the root is carried out when it is next served. A chunked file is
  withheld by its hash too: until its copy is deleted, the server serves a chunk only that file
  names as absent, and one another file shares as before. A command that needs withheld content
  from that remote fails with the new `content_withheld` code (exit 51). `audit --full` on a served
  root counts an absent withheld file apart, as `withheld` in `--json`, and the `--json` schema is
  now 32. A client that already holds a withheld file's recipe is answered for the chunks only that
  file names as for any absent object.

- **forklift-server applies redactions, and the `--json` schema is now 28.** Part of content
  redaction: after each lift or pallet deletion, the server applies every recorded redaction that
  none of its pallet heads, deleted pallets' tombstones or metadata history names (`redact list`
  names such a tombstone as what holds a redaction, `tombstones` in `--json`). It then refuses that content on every route
  as `content_redacted` (exit 49 on the client), drops it from the bundle it serves, and accepts
  lifts of history that names it without the content; `lift` no longer uploads it, nor a chunked
  file's chunks that only it names. Its handshake
  reports each redaction as pending or applied, and a warehouse takes a head the server serves over
  a redaction only while the server reports it pending. The AWS head
  refuses a lift naming content `@redactions` records, as `content_redacted`.
- **A warehouse applies a redaction once its remote has, and the `--json` schema is now 29.**
  `lower` and `clone` read which redactions the remote reports applied and, for each one nothing
  here names any more, apply it here too and delete the local copies of its content; the result
  says what was applied and deleted (`redactions` in `--json`). `redact apply` asks the remote
  first, and says when it cannot reach it. A command that applied a redaction also names the git
  mirror (`.forklift/mirror.git`), whose copy exported before the redaction nothing here deletes.
  When a `lower` or `clone` cannot finish this, it warns `redactions_unsettled` and names
  `forklift redact apply`. A `clone` or `lower` of history naming such content no longer fails
  with `content_redacted`: it never asks for the content, and applies the redaction before writing
  the pallet's head. A `lower` or `clone` that then fails, or a `lower` whose metadata did not
  lower, keeps the copies and warns; the next one that goes through, or `redact apply`, deletes
  them. `scope fetch` likewise never asks for content a redaction applied here
  removed, and widens over history naming it; and a cache's read of content its remote reports
  redacted is refused as `content_redacted` (exit 49), as `scope fetch` over it is, rather than as
  a generic error.
- **`git export` and `git mirror` leave out redacted files, and the `--json` schema is now 30.**
  A file whose redaction this warehouse applied is left out of every commit they write, with
  nothing in its place, and a folder that held only such files goes with it; before, the export
  failed on the missing content. Both name each such file with the parcels whose commits left it
  out (`redacted` in `--json`). The same parcel now exports to a different commit than it did
  before the redaction, so a mirror exported again from scratch does not fast-forward the branches
  it pushed before; commits a mirror wrote earlier keep the file.
- **The AWS head applies redactions.** A redaction that no pallet head, deleted pallet's tombstone
  or metadata history names applies on the AWS head when a request names its content, or after a
  lift or pallet deletion. The head then refuses that content as `content_redacted` on object
  reads, uploads, upload negotiation and lift commits, leaves it out of `batch` answers, and deletes
  a staged copy of it rather than promote it, or deletes the copy a promotion under way when it
  applied wrote. Its handshake reports each redaction as pending or applied. The
  first time it judges a redaction it reads every pallet, which can take a few requests; until
  then that content is not served, and the request answers `503` with `Retry-After`. The first
  request to find a redaction applied deletes the stored copies of a redacted file, every version
  of each; a lift whose history names such content no longer stored lands without it. About half an
  hour after it applied, once every request then running has ended and every bundle URL handed out
  before has expired, the next request deletes those copies again, the bundles written before, and
  the redaction's scan progress; the handshake then reports it `deleted`. A redacted chunked file's
  chunks are deleted in rounds: until a round decides which of them nothing else in the warehouse
  names, every chunk of its recipe answers `503` and is not stored again, and the round's census
  and deletion hold ref updates back with `503` for as long as they run. The round deletes the
  chunks only that recipe names, which are absent from then on, and keeps the rest. A second round
  as late as the late sweep deletes a copy landed since, then the recipe itself, and only then is
  the redaction `deleted`. A delete S3 refuses is retried, waiting longer each time. Offloaded
  bundles now live under `responses/{warehouse}/`; the `responses/` lifecycle rule expires the old
  shared keys. Deploying this needs the updated IAM policies: the verifier reads the DynamoDB table
  and deletes object versions, and the control plane keeps scan progress under `redaction-work/`
  and deletes object versions, including under `responses/` and `redaction-work/`.
- **A redaction applies only to a file's content.** A redaction naming an object a warehouse holds
  as a directory or as a chunk of a chunked file is never applied there (`redact list` reports it
  undecided, with why), and the deleter leaves such an object in place. The reads, `audit`, `heal`
  and a ref update tolerate an applied redaction's absence only where it stands as a file's content.
  `redact record` already refused to record such an object.
- **forklift-server deletes an applied redaction's content.** Right after it applies a redaction,
  the server deletes its copy of the content in its own process. While it deletes, which can
  include repacking the warehouse's packs, it answers requests for that one warehouse with `503`
  and a `Retry-After`, and cuts any download of its bundle. A deletion it cannot carry out yet, because requests did not end in time or a command
  on the server's machine holds the warehouse, is retried later and whenever the server starts.
  The write that lets a redaction apply is answered once it is applied, without waiting for
  the deletion, and the handshake reports the redaction `deleted` once no copy is left. Every `503` the server sends now carries
  `Retry-After`.
- **A redacted file's chunks are never served, and never left behind.** Until the server has
  deleted a chunked file's redacted content, it answers a chunk only that file named as absent,
  by hash or in a batch; a chunk another file shares is still served. A deletion cut short after
  deleting a chunked file's recipe now leaves a record of the chunks it still owes, so the next
  `redact apply`, or the server's next attempt, deletes them, and the server reports the
  redaction `deleted` only once they are gone.
- **forklift-server serves no content a redaction may cover until it has applied it.** A server
  that stopped between a pallet update and applying what it let apply served that content again
  once restarted, until its first apply. Now no object, batch, subtree or bundle is served while
  an apply may be owed, from just before a pallet update or deletion through the server, and from
  startup: such a request waits up to 10 seconds and is then answered `503` with `Retry-After`. An apply that fails, or cannot tell
  whether a head still names a redaction, is retried 10 seconds later, then twice as long each
  time up to 5 minutes.
- **`forklift redact` is listed, and the `--json` schema is now 27: an applied redaction's content
  is deleted.** Part of content redaction: once a redaction applies, `redact record` and
  `redact apply` delete every copy of its content in this warehouse, packed or loose, and a chunked
  file's chunks no other file shares, and say what they deleted and how much disk that freed
  (`deletion` in `--json`). A copy a checkout's base, parked work, a merge or pick in progress, or
  staged work still names is kept and listed with what holds it, until the file is taken out of
  that state and `redact apply` runs again. The deletion needs the object store to itself: while
  a command in another bay, a server or other maintenance is using it, the deletion is reported as
  owed, and the command still succeeds; any other failure to delete fails it. `redact record`
  and `redact apply` are not `forklift mcp` tools.
- **The `--json` schema is now 26: a recorded redaction is applied, or says why it is still
  pending.** Part of content redaction:
  `redact record` and `redact list` report each redaction's state, `applied`, or pending as `held`
  (naming the pallet heads or the metadata history that still hold the content), `awaiting_remote`,
  `undecided` or `ready` (for `redact apply` to apply), in `--json` as `status`. A warehouse with no remote applies a redaction once
  nothing in it names the content; one with a remote waits for the remote to report it applied. The
  new `redact apply` applies what can be after the content was taken out of the heads that held it.
  The applied redactions are kept in `.forklift/redactions-applied`. A warehouse never takes back
  content it has applied a redaction of: a `lower` of a head or metadata history the remote still
  serves naming it is refused, with a fresh clone of the remote as the way out. Content a redaction
  applied here names may be missing from the store without being damage: `audit`, `heal`,
  `store evict`, `scope prune` and ref writes pass over its absence, a cache never fetches it back
  from the remote, and `remote move` never asks the new server for it. A read that needs such
  content (`restore`, `pick`, `diff`, `blame`) is refused with `content_redacted` (exit 49), not
  as a missing object, whenever the record of applied redactions can be read. An audit that finds it absent counts it as `redacted` and passes, and a
  `heal` that rescans after an interrupted write notes how many there are. An applied
  redaction's content, and a chunked file's chunks no other file shares, are now garbage unless a
  pallet head, a metadata history or a checkout's base, parked work, merge or pick in progress, or
  staged work still names them: a `store compact --all` drops their packed copies and a collection
  their loose ones, and an incremental `store compact` leaves them loose rather than packing them.
- **The `--json` schema is now 25: `load`, `park`, merges and `pick` refuse redacted content.** Part
  of content redaction: a file whose content is redacted is refused with `content_redacted` (exit 49)
  before anything reaches the bay, when `load` would stage it, `park` would park it, `lower` would
  check it out, or a merge (`consolidate`, `haul`, `lift`'s merge) or `pick` would write it. Nothing is staged,
  parked or changed, and these refusals carry only the path and the hash.
- **The `--json` schema is now 24, with the `head_names_content` error code (exit 50).** Content
  redaction removes content from history only, so recording a redaction of content a pallet head
  still holds is refused with it, naming every head that holds the file; take the file out of each,
  then record the redaction as its next step names, usually at the parcel that held the file, as
  `<parcel>:<path>`.
- **The `--json` schema is now 23, with the `content_redacted` error code (exit 49).** Also part of
  content redaction: once a file's content is redacted, a command that would write a pallet head
  naming that content again (a `stack` of the file still in the bay, the same bytes under another
  name, a tag or other metadata carrying them) refuses with it and writes nothing. `lift` refuses
  the same way to send a head that names it, even to a remote that has not yet received the
  redaction, and says how to stack a head without the file. Metadata history that `clone` and
  `lower` take from the remote is not refused this way, even when the remote does not hold the
  redaction, since that history is never rewritten; what you stack on top of it is. Nor is a pallet
  head that `clone` or `lower` takes exactly as the remote serves it while the remote holds the
  redaction too, so the pallet that needs the file taken out can still be fetched; in a warehouse a
  Forklift server has served, that head is refused like any other.
- **The `--json` schema is now 22, with the `redaction_blocked` error code (exit 48).** It belongs
  to content redaction, which is being built and whose command is not yet offered: it refuses to
  redact content that the history of a meta pallet (`@office`, `@tags` and the rest) holds, since
  that history is never rewritten.
- **`forklift abort` abandons a consolidation or pick that stopped at conflicts.** The files the merge
  touched go back to the pallet head and nothing is left in progress; changes to other files are kept.
  It refuses without `--confirm` (exit 39) where it would throw away what you changed since, such as a
  resolution. `undo` right after such a consolidate or pick now aborts it, where it used to drop the
  record of the merge and leave the files it wrote, conflict markers included. Removing `.forklift/consolidation` or `.forklift/cherry-pick` by
  hand is no longer the way out; where the abort cannot run, `abort --keep-files` is.
- `forklift upgrade` and `forklift-server upgrade --root` upgrade a warehouse, with all its bays, to the
  format the build reads, and resume an upgrade that was interrupted. Both refuse while anything else has
  the warehouse open (exit 45). No format before the first is upgraded.
- `forklift-aws-upgrade` upgrades the warehouses of an AWS deployment in place, from an operator's
  machine under the new `iam/operator.policy.json`, and resumes an interrupted run with `--takeover`.
- Partial warehouses: a cache warehouse fetches what it reads from its origin, `store evict` frees what
  the origin holds, and `remote move` moves the origin.
- `git mirror` keeps a git remote in step, never by force.
- `git export` writes each tag as an annotated git tag over its parcel's commit, and names every tag
  it leaves out, with why (`tags` and `skipped_tags` under `--json`). Tags used to be dropped
  without a word.
- `git import` records git's tags on `@tags`, unsigned: an annotated tag with its tagger, date and
  message, without its git signature, and a lightweight tag by your configured operator. Each tag it
  leaves out is named with why (`tags` and `skipped_tags` under `--json`). Tags used to be dropped
  without a word.
- `git mirror` and a lift's mirror push the remote's tags too, as the annotated tags `git export`
  writes, once the branch holding a tag's parcel is in step. A tag git already holds as another
  object is refused and never forced, and fails `git mirror` with `mirror_incomplete`; a lift only
  reports it. Under `--json`: `tags`, `tags_in_step`, `skipped_tags`, `refused_tags`, `tags_error`.
- Pallets can be deleted and restored, locally and on a remote, on both server heads.
- A cut-off `clone` keeps what it fetched and resumes.
- A tutorial, a concepts page and a glossary in `docs/guide`.
- `--color`, and `NO_COLOR` is honoured.
- A remote refuses a pallet head holding a name no client can check out, `lower` and `clone` refuse
  one a remote already holds, and `lower --repair` and `clone --repair` stack a way back from it.
- A remote pallet delete names the head it deletes, `--at` pins it to that head, and the remote
  listing shows each pallet's head.

### Removed

- **forklift-server no longer serves `GET /v1/parcels/{parcel}/subtree/{path}`.** No client
  command sent it; a sparse clone fetches its subtrees by hash through `objects/batch`, as before.
- **The benchmark scripts `bin/bench.sh` and `bin/benchmark`, their CI job and `docs/BENCHMARK.md` are gone.**
  `proof-load perf` replaces them: it times Forklift against git on a generated history or one you
  name with `--history-from`, and judges the rows. The
  [Proof Load guide](docs/guide/proof-load.md#11-measuring-performance) describes it.

### Fixed

- When a write fails in a way that records a durability taint, the command now still puts back
  what it had already changed. This covers four cases:
  - a trust install whose office write failed puts back the previous trust anchor;
  - a metadata pallet write whose ref write failed puts its copy back as it was;
  - a pallet removal that failed puts back the undo journal and the tombstone list;
  - an `undo` whose put-back of a pallet failed moves the current pallet back to where the undone
    operation left it.

  Before, the taint left by the failure refused each of these put-backs, and the trust anchor or
  the copy was left at the new value beside an unchanged office or ref. The taint still stands
  afterwards, and `heal` is still needed. When putting the trust anchor back lands but its folder
  sync fails, the install's error now says the previous anchor was put back, not that the new one
  stands; when the install had created the trust file and its removal fails to sync, the error
  says the file was removed again.

  A pallet removal whose own journal or tombstone write failed after landing now puts that record
  back too. Before, the undo journal kept an entry deleting the pallet although the pallet was
  still there, and the tombstone list kept the pallet's tombstone. The error now says which record
  was put back. When a put-back itself fails, it says what reading the record back shows: that it
  holds what it held before but may not be durable yet, that it could not be put back, or that
  whether it was is not known.

  An `undo` of a `stack`, `consolidate` or `pick` whose put-back of a pallet failed in this way
  could not be retried, even after `forklift heal`: the current pallet and the bay's position were
  both left put back while the journal still held the undone operation, so every retry refused
  with "has moved since that operation".
  Now the current pallet is moved back where the operation left it, and the retry runs once
  `forklift heal` has cleared the taint. The move still runs the redaction check, and in a served
  or sparse warehouse it still reads the pallet's history; a read the taint refuses leaves the
  undo as before, and the error then says a retried undo will not pass its check. The same holds
  when the fetch scope changed while the undo ran, or when the record of sealed or unavailable
  content lost an entry, as it does in a cache warehouse when sealed content is read back in
  meanwhile: the pallet is then moved back by an ordinary checked write, which the taint refuses.
- `heal` no longer refuses forever over a durability taint that records a lost metadata ref copy
  (`.forklift/meta-copy/<name>`), as a crash can leave one after a metadata pallet's first write.
  When the ref beside it is a file holding a parcel hash, that part of the taint is resolved, and
  the copy is then treated as a lost copy on a warehouse with no taint: `heal` sets it to the ref's
  bytes, or says why it could not. Before, the taint named the copy as a shape it did not
  recognize, and the copy was left until the taint cleared, so every run refused the same way. A
  copy lost beside a ref that is lost, damaged, unreadable or not a file still refuses, and says
  which.
- Under `--json`, a `heal` whose reachability check cannot finish, with no damaged ref or inventory
  refusing, now says what the run repaired in the error's `message`, after the failure. Before, that
  error carried only the failure, so the repairs went unreported though they had happened. A heal
  whose inventory step fails with an uncoded error (a shard store that cannot be listed, a rebuild
  write that fails) now says every repair the same way, where it said only the recovery of an interrupted
  operation. A failed reachability check after a run that did nothing no longer prints "Nothing to
  heal" before it.
- A `heal` that clears a durability taint and then stops, for damaged tombstones or another refusal
  later in the run, now says in its error the taint it cleared, with the paths it stored again or
  resolved and its notes, and every repair made after the taint in the order the run made it: a
  trust file restored only then, and, when a damaged merge record's repair fails after it, the
  recovery of an interrupted operation. Before, those stops left them out, and with the taint's
  files gone no later run reported them. A note quoting a refusal is said once, as prose.
- **`heal` restores a damaged record of a merge in progress from a copy kept beside it.** Every
  write of a consolidation's state, its out-of-scope skeleton or a pick's state now leaves a copy
  (`consolidation-copy`, `consolidation-skeleton-copy`, `cherry-pick-copy`), and `forklift heal`
  writes a damaged record back from it when the copy reads and agrees with where the bay stands,
  reporting it in `merge_state_restored` (`--json` schema 55). The refusal of a damaged record
  names `forklift heal` while that can run, and `abort --keep-files` (or `abort` for the skeleton)
  once no copy can be used. Under an interrupted merge or pick, heal removes a record no copy can
  restore, since finishing that operation drops it anyway. A consolidation records its skeleton's
  digest, so a skeleton left by another merge is refused as damaged. The consolidation state's
  format changes: a merge in progress written by an older Forklift is refused.
- **A merge no longer starts over one in progress.** `haul consolidate`, and the merge a `lower`
  makes, refuse while a consolidation or pick is in progress in the bay, and a `lift` there makes
  no merge of its own and stops as diverged; `haul consolidate` over a conflicted consolidation
  whose files were resolved used to write a second merge's record over the first one's.
- **A server's `500` no longer names its files.** A request that failed on a fault of
  `forklift-server`'s own answered with the failure's own message, which could carry the server's
  absolute paths, such as a pack index that does not parse or a damaged ref. A `500` now carries a
  fixed message and no code, and the cause goes to the server's log. A batch fetch that would have
  to carry an object over the ceiling is refused with a `422` and its code, not a `500`, so the
  client stops on it rather than retrying. A warehouse whose locks could not be taken is refused
  with a `503` that no longer names the lock file or the process holding it.
- **A damaged record of a merge in progress is refused with a command that ends it.** A garbled,
  cut or digest-mismatched consolidation or pick state file was refused in prose that said to
  remove the file by hand. Every reader now refuses it with `merge_state_damaged` (exit 68,
  `--json` schema 54) and names `forklift abort --keep-files`, a new form of `abort` that drops the
  record without reading it and leaves every file as it is, so the next `stack` records them as
  an ordinary parcel and `undo` reaches the operation before the merge. A damaged consolidation
  skeleton names the plain `forklift abort`. The abort's own refusals, for a pallet that moved or
  a source parcel it cannot read, name `abort --keep-files` too.
- **A lost or damaged checkout scope is refused with a remedy, and `heal` restores it.** The scope
  file of a sparse warehouse's working tree or of a scoped bay, deleted or garbled, was refused with
  only a way out that discarded the checkout: clone again, or remove the bay and add it again.
  Every reader now refuses it with `checkout_scope_damaged` (exit 67, `--json` schema 53). Every
  scope write leaves a copy (`scope-copy`) beside the scope file, and `forklift heal` in that
  checkout writes it back, unless the copy names a path the warehouse never fetched or would leave a
  committed or untracked file outside the checkout, where it could never be stacked again. Then it
  changes nothing and names the files, and `heal --scope <path>` sets the scope to the paths the
  checkout held, refused unless they match what is on disk and in the store.
- **A server's damage refusals no longer name the server's directories to clients.** The
  `tombstones_damaged` refusal a client got quoted the full path of the damaged tombstone list or
  served mark on the server's disk. The server now sends a fixed message for every damage code, as
  it already did for six of them, and keeps the reader's own message, path included, for its log.
  When only the mark was lost or damaged, `forklift-server tombstone repair` also reported "Kept 0
  entries of the old list" over a list it had left untouched. It now says the list read whole and
  was left as it was.
- **A damaged inventory metadata file is refused with a remedy, and `heal` rebuilds it.** The
  inventory's record of which folders hold staged content, cut short, zeroed or with a byte
  flipped, failed `load`, `unload` and `status` with an uncoded error, and `heal` found nothing to
  heal. Every reader now refuses it with `inventory_damaged` (exit 66, `--json` schema 52), and
  so does a deleted record while the staging area still holds staged content. `forklift heal`
  rebuilds the record from every folder it can find whose staging is on disk: the damaged file's
  lines that still read, the working tree, the tree the bay's files stand on, and the trees an
  interrupted operation names. It writes the record only when that accounts for all the staging on
  disk and it read every one of those trees. Otherwise it writes nothing, finishes its other
  repairs, and refuses naming what it could not account for. `heal --inventory` then writes the
  record anyway, and the staging no folder matched is no longer registered.
- **A damaged configuration file is refused with a remedy, and `heal` mends the warehouse's
  own.** A configuration file holding bytes that are not text, or zeroed, failed every command
  with a raw read error and no way out. Every reader now refuses it with `config_damaged` (exit
  65, `--json` schema 51), as it does a file cut short so that its last line no longer parses. In
  the warehouse's own file the next step is `forklift heal`, which rewrites the file when the
  damage provably cost no setting, for instance a garbled comment, and the rest of the file is
  valid, keeping its comments. Otherwise
  it changes nothing and names the entries the damage may have touched. `heal --config` then keeps
  only the entries the damage provably spared. A section that loses one entry loses all of them,
  so a remote's address is never kept without its token. The damaged bytes are set aside as
  `config/warehouse.toml.damaged`, with the file's own permissions. The report names what was
  dropped by line and key, and never prints a value. A file with a mistake made by hand keeps its
  "fix the file by hand" refusal and does not stop `heal`, which never writes over a file it
  cannot read.
- **A lost or damaged bay position is refused with a remedy, and `heal` rebuilds it.** A bay
  whose position file was deleted, cut short or garbled failed every command with a raw read
  error and no way out, and `prepare` over a deleted one silently put the bay on `main` with no
  parcel. Every reader now refuses with `bay_position_damaged` (exit 64, `--json` schema 50) and
  `forklift heal` as the next step, and `prepare` refuses the same way. Each position write also
  leaves a copy beside it, and `heal` rebuilds the position from that copy, or from an
  interrupted operation. With neither it lists the pallets, and `heal --position <pallet>` puts
  the bay on that pallet's head. A position or copy a newer version wrote is never written over.
- **A remote refuses history that names an object as a kind it is not.** A ref update whose new
  history named a folder's tree or a large file's piece at a file entry, or a plain file's
  content as a large file or one of its pieces, was taken, and the pallet then pointed at history
  nothing could read. Both servers refuse it with `object_kind_mismatch` (exit 63, `--json`
  schema 49), naming the object, the path and both kinds. The serverless server records each
  object's kind as it stores it, which is AWS format 3: a format 2 warehouse is refused and must
  be made again under a new id.
- **`heal` no longer sends you to a backup over a load you never stacked.** When the store lost the
  content of a staged file, `heal` refused with exit 58 (`objects_lost`), saying history still
  reached it and telling you to restore from a backup or clone afresh. Now `heal` stores lost
  content again from the working tree wherever a file there still holds it, staged or stacked, and
  lists the paths. When only staged content reaches what is still lost, the refusal says so, names
  the paths, and its next step is `forklift unload` on each; content another bay stages is named
  with the bay, whose own `heal` stores it again or names what to unload there. `--json` carries
  `restored_from_working_tree`, and `objects_lost` gains `staged_only`, `staged` and
  `staged_elsewhere`.

- **A refused `scope fetch` or `clone --only` of a file no longer stores it.** `scope fetch
  docs/guide.md`, naming a file rather than a directory, was refused as not a directory but had
  already downloaded the file across its history, so `show` then read it outside the fetch scope.
  `clone --only docs/guide.md` did the same, and a resume of that clone at a corrected scope kept
  the file. Both now check the path before storing anything, and the refusal stores nothing of it.

- **An object folder that cannot be listed is reported, not read as empty.** Matching a short hash
  or salvaging a damaged pallet ref skipped a folder of the object store it could not list, so a
  typed prefix was answered "No object with hash … exists", and `heal` could judge that a damaged
  ref's bytes named nothing and rebuild it from older evidence. Both now fail naming the folder,
  and `heal` leaves such a ref alone, saying its salvage is unknown. The store's other listings
  now name the folder too.

- **A power loss no longer drops a folder that was just written into.** The first object, ref or
  shard written into a new folder under `.forklift` synced the folder but not its own entry in the
  folder above, so a power loss soon after could drop the folder, and every file in it, while a
  pallet head written later still named what was in it. Clients and forklift-server alike now
  sync every folder from the one written into up to `.forklift`.

- **The AWS head no longer stores a file or a chunk under a second hash.** A client uploading
  straight to the AWS head's storage could respell a file's or a chunk's header, and the head
  stored it under the new hash, where a redaction of the file's own hash did not reach it. The head
  now refuses any upload that is not exactly the bytes Forklift writes, as forklift-server does;
  a batch fetch of such an object already in a bucket is refused with a `422` naming it, not an
  internal error.

- **On Windows, a delete no longer fails at once on a file another program is reading.** A search
  indexer, an antivirus scanner or an editor that opens a file without letting others delete it
  used to fail any Forklift command that removed that file or a folder holding it, such as a
  sweep, a repack or a bundle install. Deletes now wait up to a second for such a hold
  to lift, as renames already did, and fail with the same error only if it outlasts that.

- **A load, unload or remove next step for a path starting with `-` runs as printed in Windows
  PowerShell.** Such a path used to follow `--`, and Windows PowerShell 5.1 splits an unquoted
  argument that starts with `-` at its first dot, so `forklift unload -- -draft.txt` reached
  Forklift as two paths. These next steps now spell it `./-draft.txt` instead, on every platform.

- **Three more next steps quote the path they name.** The next step of an `incomplete_load`
  refusal, of `restore` on a file whose removal is staged, and of an inventory that cannot be read
  used to paste the path in bare, so a path with a space or a character a shell reads specially
  gave a command that did not run as printed. They now quote it for a POSIX shell and spell a path
  starting with `-` as `./<path>`. On Windows, where no quoting reads such a path as one argument,
  they print a placeholder instead and say to quote the path for your shell. All three now name
  the path relative to the working tree's root and say to run the step from there, and an
  `incomplete_load` over several paths names a command for every one of them, not only the first
  three.

- **The `restore --confirm`, `scope prune --confirm` and `bay remove` next steps run as printed on
  Windows.** They used to single-quote a name for a POSIX shell, which `cmd` reads literally, and
  to put a name starting with `-` after `--`, which Windows PowerShell 5.1 splits at its first dot.
  A path starting with `-` is now spelled `./<path>` on every platform. On Windows, a path is
  printed as typed when it holds only letters, digits, `.`, `_`, `-`, `/`, `\` and a drive's `:`;
  any other path becomes a `<path>` placeholder, and the message names the path to put in its
  place. A bay folder whose name holds anything else there, or starts with `-` and holds a dot,
  is left to be removed by hand, as one no shell can spell already was. The `staged_content_missing`
  next steps and the refusal for an unreadable chunked file's recipe now also say to run the
  command from the working tree's root.

- **A read from a remote no longer fails on a connection the remote had already closed.** When a
  remote or a gateway in front of it closed an idle connection the client was about to reuse, the
  `GET` of an object, a subtree, a signature, a bundle or the handshake failed the command with
  "connection closed before message completed". The client now sends such a `GET` again, up to
  three times in all, when its connection closed within a second of the request starting. A
  connection that closed later, after the remote had worked on the request, is not resent, nor is
  any other kind of request, or a `GET` that timed out or could not connect.

- **forklift-server writes plain-text logs when stderr is not a terminal.** Its logs used to carry
  colour escape sequences into a log file or a service manager's journal; they are now coloured
  only on a terminal.

- **`git export` and `git mirror` in a server's own root leave out content its host withholds.**
  Run in a forklift-server root, both used to write a file the host withholds into git while the
  server still held its copy, and to fail on the missing object once it had deleted it. They now
  leave the file out of each commit they write, as they do for an applied redaction, and name it
  with the host's statement (`withheld_here` in `--json`); a mirror's commits already in git, and
  a branch whose tip has not moved, keep it. A takedown or redaction recorded while either runs
  fails the export, or that run's mirror pallets, before git holds it; run it again. `--json` schema is now 41.
- Every parcel `stack` and `pick` record has a description. `stack` without `-m` refuses instead of
  recording a parcel with none, unless it completes a consolidation or pick in progress, which
  supplies one. `pick` of a parcel with no description needs `-m`. A blank `-m` on `stack`, `pick`
  or `deliver` is refused.
- `forklift-server` authenticates from a request's headers before reading its body, so an
  unauthenticated peer can no longer hold server memory by opening large uploads.
- A served root's meta pallets never move back to an ancestor.
- Every `.forklift` folder ignores itself for git, so `git add -A` in a colocated repository never
  stages a remote token.
- A read that overlapped a `store compact` in another process no longer refuses a healthy
  parcel's signature as unreadable. The compact had merged the signature table the read was about
  to open, and deleted it. The read now looks again and finds the signature in the merged table.
  A pack file that was replaced and deleted the same way is no longer read as a damaged pack.
  A pack folder that keeps changing under eight reads in a row is refused with "kept changing
  while it was read"; run the command again once the compact has finished.
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

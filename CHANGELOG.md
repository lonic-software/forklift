# Changelog

Notable changes to Forklift, for people who use it. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Before 1.0, any release can change commands,
on-disk formats and the wire protocol, with no migration.

Entries are written by hand in the pull request that makes the change, under **Unreleased**. What
earns one is listed in the [contributor guide](docs/guide/contributing.md#8-keeping-docs-in-sync).
`bin/release` turns that section into the new version and refuses a release if it is empty.

## [Unreleased]

### Changed

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
  spelling and refuses any other, when reading and when a parcel would add one. A cell value can
  no longer hold a float or a date-time. A cell written by an earlier build that holds a string with
  a quote, a backslash or a line break is spelled differently and now reads as malformed.

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
  hand is no longer the way out, except where the abort cannot run, and those refusals say so.
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

### Fixed

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

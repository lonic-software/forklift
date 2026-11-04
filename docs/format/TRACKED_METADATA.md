# Tracked metadata — users, keys and the trust anchor
Tracked metadata (FORK-15) is Forklift's substrate for identity: users (FORK-14) and
signing keys (FORK-12) are ordinary blobs in ordinary trees, so they inherit hashing,
signing and transport for free. The `office` command manages them; the `audit` command
verifies them.

## The office pallet
Identity records live on the **office** pallet, a *meta pallet*: a real pallet (it
hashes, signs and transports like any other) that lives in its own namespace
(`.forklift/meta/office`, not `.forklift/pallets/`) rather than being a reserved name.
Its parcels are synthesized (no working directory or inventory is involved) and each
snapshots the *full* current state of every record, exactly like a code parcel snapshots
the tree. The office history is therefore the complete, signed audit trail of every user
and key change.

Because it is namespaced, the office does not reserve any user pallet name — `office` is
a legal working pallet, distinct from it. The office is addressed with the `@` qualifier:
`history @office` and `audit @office` read it, `pallet list --all` lists it. Working-pallet
operations (`pallet create`, `shift`, `consolidate`, `stack`) only ever see the user
namespace, so they never touch it; the content guard (a tree with a top-level
`.forklift` entry, in any case, is never materialized, and no working-directory
write lands inside `.forklift`) stays as defense-in-depth.

## The manifest pallet (FORK-9 post-metadata)
The **manifest** is a second tracked-metadata meta pallet (`@manifest`, at
`.forklift/meta/manifest`) holding *post-metadata*: signed statements attached to a
parcel after the fact — approvals, review notes, machine-authorship provenance (§7.2) and
delivery records (§7.3). An entry **references** its subject parcel and never mutates it
(the §4.4 immutability invariant). It is written in the meta-tree format below; its cells
are described under [`@manifest`](#manifest).

## The reserved tree namespace
Office parcel trees place the records under:

```
.forklift/tracked/users/<blake3(identifier)>.toml
.forklift/tracked/keys/<key-id>.toml
```

The namespace is collision-proof by construction: the `.forklift` folder is the
warehouse root and is never tracked by `load`, so no user file can ever occupy these
paths. Trees carrying a top-level `.forklift` entry are refused by every operation that
materializes or merges trees.

## The meta-tree format
`forklift-core`'s `meta_tree` module defines the format of every meta pallet but the office:
`@kv`, `@manifest`, `@tags` and `@haul`. A meta tree is the pallet's whole state, and its chain
is linear (a parcel has at most one parent), as with the office:

```
.forklift/tracked/format          a blob reading exactly "forklift-meta 1 <kind>"
.forklift/tracked/cells/<path>    one cell per path (cells/ is omitted when empty)
```

`<kind>` is `kv`, `manifest`, `haul` or `tags`. Every path component uses only ASCII letters,
digits, `.`, `_` and `-`, and none may begin with `.` or `-`. Subjects that are hashes fan out
by their first two hex characters (`cells/ab/<hash>/...`). Each cell is a TOML envelope:

```toml
signer = "<key id of whoever wrote this value>"
schema = "kv.text 1"
[value]
text = "..."
```

The envelope has exactly one stamp — `signer`, a 64-hex key id, or `operator`, a non-empty
operator identifier — plus `schema` and `[value]`, and no timestamp. A writer writing a value
therefore always produces the same bytes, and two writers of the same value produce different
cells. A cell with both stamps, or neither, is malformed.

Reads are tolerant per cell. A cell with an unknown schema, or one that does not parse, is
reported as that cell's row, and the other cells still read. A missing or unknown `format`
refuses the whole pallet, naming it as written by an older or newer forklift. Writes leave
every path they do not name byte-for-byte unchanged.

Which parcels a pallet accepts depends on the warehouse's trust state:

1. **Never enrolled** — no trust anchor and no office. Every parcel is unsigned. Its `Author`
   and `Stack` actions name one operator, and every cell it adds or changes is stamped
   `operator` with that identifier. A parcel carrying a signature, or a cell stamped with a
   key, is refused: with no office there is nothing to verify it against. The pallet rules
   that judge roles (who may cut a tag, who may record a merge) do not apply, because there
   are no roles; the rules that judge authorship (only the opener writes `opened`, a review
   lives under its writer's directory) apply to the declared operator. Nothing binds that
   operator to whoever lifts the parcel, so these rules keep a parcel consistent with the name
   it declares; they do not establish that the name is true.
2. **Anchor lost** — no trust anchor while an office exists. Every write and every content
   check refuses until trust is restored.
3. **Trusted** — an anchor and an office. Every parcel stacked after trust was established must
   be signed, and its signature must verify, inside the distrust boundary if its key is revoked.
   Every cell it adds or changes is stamped `signer` with its own signing key, and the pallet
   rules hold in full. An unsigned parcel is admitted only when it is reachable from the
   anchor's `boundary` (exact ancestry), and is then judged as in state 1: written before the
   warehouse enrolled. One that is not reachable is refused, or, when a boundary parcel is not
   present locally, refused as unresolvable rather than as stacked after trust.

In every state a parcel's format must stay its parent's, and its changed envelopes must parse,
in a canonical tree with no empty subtrees. `verify_meta_content` checks each parcel this way,
so by induction over the linear chain every cell at an accepted head names the writer that put
it there: verifiably when a signed parcel wrote it, and only as declared when an unsigned one
did (in a warehouse that never enrolled, or before a trusted one's boundary). A client
writes in the state it reads, and its write refuses if an office or anchor appears before the
head moves.

Diverged heads are not merged. The local net change since the common ancestor is replayed onto
the remote head as one new parcel, and each replayed cell is re-stamped by the current writer.
A cell already carrying the writer's own stamp is kept as is. Under trust a stamp may be
re-signed only when it resolves to the writer's own operator: an `operator` stamp naming it, or
a key stamp under one of its keys that is active or was retired, never one revoked as
compromised; any other re-stamp refuses. A local cell the remote already holds re-signed by
this rule — same schema and value — is the same change, not a conflict with it. Paths that both
sides changed differently refuse until each is resolved, keeping ours or theirs; keeping theirs
at a subject resolves every conflict beneath it.

Under trust, a lift of a meta pallet whose new parcels include an unsigned one outside the
boundary (metadata recorded before the remote enrolled) replays it the same way instead of
lifting it, onto the remote head, or as a root parcel when the remote holds none. A replay that
keeps theirs at every local path onto a remote with no head still stacks that root, with no
cells, so the signed head replaces the unvouched one.

A server, the serverless head and a client adopting or lowering a pallet on this format run
`verify_meta_content` in the trust state they hold, from the head they hold. The meta pallet
names are a fixed list — `office`, `manifest`, `haul`, `tags`, `kv` — and any other `@name` is
refused, so a newer forklift's pallet is never stored unchecked by one that cannot read it.

### `@kv`

Custom metadata: free-form text keyed by parcel and key.

```
cells/<s[0..2]>/<s>/<key>         schema "kv.text 1", value { text = "..." }
```

`s` is the full hash of the parcel the value is about (it need not be held). A key is one path
component of at most 128 bytes; `text` is the only field of the value, UTF-8 of at most 64 KiB.
Any operator in the office except a reader may set or unset any cell (the reader rule is
enforced at transport and by the CLI, not in the content check); in a warehouse that never
enrolled, anyone may. Conflicts are resolved per
`<s>/<key>`, or per `<s>` for `theirs`.

### `@manifest`

Post-metadata about parcels. Every cell sits under the directory of the operator who wrote it.

```
cells/<s[0..2]>/<s>/approval/<op>         schema "manifest.approval 1"
cells/<s[0..2]>/<s>/note/<op>/<id>        schema "manifest.note 1"
cells/<s[0..2]>/<s>/provenance/<op>/<id>  schema "manifest.provenance 1"
cells/<s[0..2]>/<s>/delivery/<op>/<id>    schema "manifest.delivery 1"
```

`s` is the full hash of the subject parcel, and `op` the `Blake3` of the writer's operator
identifier, as office user records are named. An approval is one per operator: the cell `op`
itself. The other kinds are multi-valued: `id` is the `Blake3` of every field of the entry, each
length-prefixed. One writer's identical entries therefore collapse into one cell, and the same
text from two writers never shares a cell. The values, each with exactly these fields:

```toml
# approval, note
body = "LGTM"                       # may be empty, e.g. a bare approval

# provenance: which model produced the parcel, with which tool, in which session
body = "generated the auth module"
model = "claude-opus-4-8"           # required — the compliance-critical field
tool = "claude-code"                # optional
session = "sess-42"                 # optional
transcript = "b3sum:…"              # optional — a prompt/transcript fingerprint
recorded_at = 1782205234            # display only; only provenance carries a time

# delivery: the subject is a clean squash of a draft pallet's checkpoint trail (`deliver`)
body = "add the feature"            # the delivered parcel's message
source = "draft/feature"            # the draft pallet the trail came from (kept)
trail_head = "e6ee0a58…"            # the trail tip — `history` on it walks the full trail
checkpoints = 3                     # how many checkpoints were squashed
```

There is no author field: the author is the operator the cell's stamp resolves to — the owner of
its key under trust, so it cannot be forged, or the declared operator when the warehouse never
enrolled. The pallet rule, checked wherever the content check runs: a parcel may add, change or
remove cells only under its own writer's `op`, whatever schema they carry, and an
entry of a known schema only at the path its kind and content derive. A cell of a kind or
schema this build does not know is listed as unreadable, and a query treats it as possibly
matching (an unknown, never an absence); its ownership is still enforced. Conflicts are
resolved at any path from `<s>` down to a cell.

### `@tags`

Release labels, one cell per name, signed under trust:

```
cells/<name>                        schema "tags.tag 1"
```

```toml
subject = "0f1877dd…"               # the full hash of the tagged parcel
message = "first release"           # may be empty
tagged_at = 1782205234              # display only
```

A name is one path component of at most 128 bytes. Two rules hold. Under trust, a parcel that
adds a tag must be signed by an operator who is an admin in the office it is admitted against
(a warehouse that never enrolled has no roles, so anyone may tag): when a
client writes or replays it, and when a server or the serverless head moves its head. A client
adopting history a server already admitted does not re-check the role, so a later demotion
never wedges the history it admitted. And no parcel may change or remove an existing tag, so a
tag name is immutable everywhere. Two tags created concurrently under one name are a conflict
at `lower`: keeping `theirs` drops the local tag, and keeping `ours` is refused by the
immutability rule.

### `@haul`

Merge proposals (pull requests). Each haul is a directory of cells:

```
cells/<h[0..2]>/<h>/opened              schema "haul.opened 1"
cells/<h[0..2]>/<h>/state               schema "haul.state 1"
cells/<h[0..2]>/<h>/review/<op>         schema "haul.review 1"
cells/<h[0..2]>/<h>/comment/<op>/<id>   schema "haul.comment 1"
```

`h` is the haul id: the `Blake3` of every field of the `opened` value, each length-prefixed. The
opener and the time are among them, so two hauls never share an id. `op` is the `Blake3` of the
writer's operator identifier, as in `@manifest`, and a comment's `id` the `Blake3` of its fields.
The values, each with exactly these fields:

```toml
# opened
source = "feature"                  # the pallet proposed for merging
target = "main"                     # the pallet it merges into
title = "Add X"
description = "why"                 # may be empty
opener = "alice@example.com"        # the operator who opened it — must be the writer's
opened_at = 1782205234              # part of the id; never a security input

# state — the status and the head it is of, in one cell
state = "merged"                    # open | closed | merged
head = "0f1877dd…"                  # the full hash of the proposed source head (once merged, the head merged)
merge_parcel = "9a03c1e2…"          # present exactly when merged

# review — one per operator, replaced when they review again
verdict = "approve"                 # approve | request-changes | comment
body = "LGTM"                       # may be empty
head = "0f1877dd…"                  # the head reviewed; a review of an earlier head does not count
reviewed_at = 1782205234            # display only

# comment
body = "…"
commented_at = 1782205234           # display only
```

The pallet rules, checked wherever the content check runs:

- `opened` is written once, by the operator it names, at the id its value derives, in the same
  parcel as a `state` that reads open, and is never changed or removed.
- No other cell may exist in a haul directory without its `opened`.
- `state` is never removed, and never changed once it reads merged. Anyone may close or reopen
  at the current head; only the opener moves the head, to any parcel (a review of an earlier
  head stops counting). Under trust, a state that reads merged is admitted only from an
  operator whose user record may write the haul's target — an admin, a writer of every pallet, or a writer granted
  the target — since the record says the target took the haul. The rule binds where a parcel is
  first admitted (stacking it, a remote accepting its lift); a warehouse adopting a remote's
  already-admitted @haul audits only the signatures and the other rules, so a merge whose signer
  has since lost write on the target stays. A fresh remote admits the whole history against its
  current office, so such a merge is refused there, naming its signer and target; an admin who
  grants the signer write on the target again lets it lift. Because the status and head are one cell, a merge always records
  the head it merged, and an update racing a close or a merge is a conflict on that cell rather
  than two clean changes.
- `review/<op>` and `comment/<op>/…` are written or removed only by the operator `op` names.
- `opened` and `state` hold exactly their own schema, since the rules read them, and a
  strictly valid value. A cell name not listed above is refused, so no cell is stored that no
  rule covers.

For an unsigned parcel, "the operator" in these rules is the operator the parcel declares, so
they hold the parcel to its own declaration; only a signature holds the writer to it.

A review or comment of a newer schema is carried, and listed as unreadable. When a haul's
`opened` or `state` cannot be read, the whole haul is listed as unreadable and every
command addressing it refuses. Conflicts are resolved at any path from `<h>` down to a cell;
keeping `theirs` at `<h>` drops a locally opened haul whole. Any two concurrent changes to
a haul's state — a close, a reopen, an update, a merge — are a conflict on `<h>/state`. Keeping
`ours` over the opener's update replays the old head, so only the opener can; anyone else keeps
`theirs` and runs the merge or close again.

A `merged` status names the parcel the merge stacked. Under trust the record is a signed claim by
a writer of the target; an unsigned one is only as honest as the operator it declares, and in a
warehouse that never enrolled anyone may record it. Either way, a reader judges it on read
against what its own warehouse holds, as its merge proof:

- `verified` — the parcel contains the haul's head, and the local target contains the parcel;
- `refuted` (`head_not_contained`) — the parcel is held here and does not contain the head;
- `unproven` — neither, because the parcel is not held here (`parcel_not_held`), the target has
  no head here (`target_not_held`), the target here does not contain the parcel
  (`not_on_target`), or the history the answer needs could not be read (`history_unreadable`).
  None of these says the record is false: a warehouse behind its remote reads the same.

A lift of `@haul` (and the replay a diverged `lower` lifts) holds back a merge record whose parcel
is in the history of the local target while the remote's target is not known to hold it — the
remote has no such pallet, its head is not held here, or it is held without the parcel — so a
remote never holds a final merge before the parcel it names. `lift` lifts `@haul` after the
working pallet, whatever that lift's outcome, counting a head it just landed; the other meta
pallets lift first. A held `@haul` fails as a meta pallet naming each haul, parcel and target. It
goes once an operator who may write the target lifts it there (lowering it first when the
remote's head is not held here). A record whose parcel is on no local target, or whose target is
not held here, goes through: waiting would not bring that parcel.

## Record formats (TOML)
A user record:
```toml
identifier = "mate@lonic.net"
enrolled_at = 1782205234
role = "writer"            # "admin" | "writer" | "reader"; absent = admin (pre-privilege records)
identity_root = "<key id>" # the key the office pins for this operator (§8.5)
pallets = ["main"]         # writer-only: the pallets they may move; absent = all
class = "agent"            # "human"(default, absent) | "agent" | "bot" | "service" (§7.1)
supervisor = "alice@lonic" # automated identities only: the responsible human (agents require one)
```

Roles (FORK-10) are tracked, signed metadata like everything else in the office:
**admin** manages the office (admissions, roles, others' keys) and may move any pallet;
**writer** moves working pallets (all, or the `pallets` grants) and manages their own
keys; **reader** moves nothing (key self-service still applies). **The `pallets` grant
scopes working pallets only:** any operator who is not a `reader` may transport a ref
update to any meta pallet whatever their grant list says, because the server never consults
the grant for one. What lands in
`@office` is still constrained, per parcel, against the *signer*'s role. A record without a
`role` predates privileges and reads as admin — exactly the pre-privilege behavior.

**"May move any pallet" (and the `writer`/`reader` transport rules above it) is a transport
rule a server enforces only for a caller it resolves to that operator's own identity** — a
per-operator token, or an `authentication` hook — never a property of the signature itself.
`docs/SERVER.md`'s "Per-operator tokens" section is the authoritative statement of what that
transport rule reaches, and of what the content-level audit checks independently of it. In
short: a validly-signed, non-revoked parcel on an ordinary pallet passes
`audit`/`verify_pallet_history` regardless of its signer's role or `pallets` grant — neither is
checked there (the grant is consulted by the server's transport gate, `authority_utils::may_move`,
and, for a `@haul` merged-state cell only, by the content rule `haul_utils::may_record_merge`;
never for an ordinary pallet's history) — but the **office** pallet is the exception: every
office-modifying parcel is checked in content audit against its signer's role
(the office chain walk, `verify_office_chain`, which a client and `audit` run as well as both heads), regardless of how, or whether, the caller authenticated. A server
run with only the shared static token, an `--open` server, or the AWS serverless head does not
resolve a caller to an operator identity at all, so none of them run the *transport* rule above
— but that office content-level check still applies.

`class` (§7.1) is *provenance*, orthogonal to `role` (*authority*): human (the default,
so human records keep their historical shape), agent, bot or service. Because the class
rides in the admin-signed record, "an agent authored this, supervised by <human>" is
forge-proof and offline-verifiable. Rules enforced at `office admit`: an **agent**
requires a `supervisor`, and a `supervisor` must be an enrolled **human** (automation
cannot supervise automation). The class is set at admission and never changed by
`office role`. Automated identities are expected to hold *passphraseless* keys — they
sign autonomously under their own marked identity — while human identities hold
*passphrase-protected* keys, so a process without the passphrase (an unattended agent)
cannot sign as a human. Principle: automation signs *as itself*, never as a human.
Office changes by non-admins must be self-service (only their own keys change); remotes
enforce this per office parcel against the *signer's* role in the previous state, so a
parcel can never grant its own author privileges. The genesis operator is an admin, and
the office always retains at least one (lockout protection in `office role`).

A key record, filed as `keys/<key-id>.toml` (`key-id` = the Blake3 hex hash of the raw
Ed25519 public key bytes):
```toml
operator = "mate@lonic.net"
public_key = "9d61b19d…"   # Ed25519, 32 bytes, hex
issued_at = 1782205234
retired_at = 1782206000    # absent while the key is active
revocation_reason = "compromise"   # "retirement" or "compromise"; written with retired_at
distrust_boundary = ["<pallet head hash>", …]   # absent while the key is active
authorized_by = "<key id>"         # the operator's own key (an endorsement), or an admin's
endorsement = "<hex>"              # authorized_by's Ed25519 signature (payload below)
proof_of_possession = "<hex>"      # this key's own signature (payload below)
```

`authorized_by`, `endorsement` and `proof_of_possession` are mandatory: a record missing any
of them does not parse. The endorsement signs the text
`forklift key endorsement v1\nkey: <public_key>\noperator: <operator>\nauthorized_by: <key id>\nissued_at: <issued_at>\n`,
and the proof of possession signs `forklift key proof-of-possession v1\nkey: <public_key>\noperator: <operator>\n`,
so neither can be moved to another operator. Only an operator's identity root (their first key) may authorize
itself; every later key is endorsed by a key of theirs that chains to that root, or by an
admin's. A revoked key's signatures are
valid only on parcels reachable from its `distrust_boundary`, decided by ancestry, never by
`retired_at`.

A key record does not store its id; readers derive it from `public_key`, and refuse a record
that has a `key_id` entry. Every lookup goes by key id, while the endorsement and
proof-of-possession sign only the public key, so a stored id could name one key over another
key's material. Readers that build the office state refuse a record filed under any name but
its own subject's (`<key-id>.toml`, `<blake3(identifier)>.toml`), naming the file and the one
it belongs in. They also refuse a folder inside `users/` or `keys/`, and a `.forklift`,
`tracked`, `users` or `keys` entry that is a file rather than a folder. Since tree entry names
are unique, no subject can have two records. The writer refuses, before storing anything, a state no reader would accept.
Collecting trust pins for gc, heal and recovery is the one read that checks neither file names
nor nested folders: it keeps the pins of every record under `keys/`, at any depth. It still
refuses a file in place of `.forklift`, `tracked` or `keys`, which would otherwise read as an
office with no pins.

Retired keys are **retained forever**: they still verify the parcels that were signed
while they were active. The private halves never enter the warehouse; they live in the
operator's `~/.forklift-keys/<key-id>.key` (mode 0600, `FORKLIFT_KEYS_DIR` overrides
the location).

## The trust anchor (`.forklift/trust`)
Trust is established by the first `office enroll`: the genesis office parcel introduces
the first user and key and is self-signed by that key (trust-on-first-use). The trust
file records it:

```toml
version = 1
genesis = "<genesis office parcel hash>"
enabled_at = 1782205234
boundary = ["<pallet head hash>", …]
prior_genesis = "<hash>"   # re-genesis only: the genesis of the chain this anchor replaced
adopts = "<hash>"          # re-genesis only: the old office head, kept as attested history
```

- `version` is the shape of the file. A trust file with no version, or another one, is refused
  rather than read, since a TOML reader would otherwise pass over a key it does not know.
- `genesis` anchors the office chain; `audit` refuses an office history that does not
  reach it. The genesis has no parents, and every parcel of the chain has at most one and a
  tree holding only `.forklift/tracked/users` and `.forklift/tracked/keys` with their record
  files. `audit` refuses anything else.
- `adopts` is held to that same shape, parcel by parcel to its root, wherever a remote's
  anchor is taken, without checking signatures. Local re-genesis does not check it, so it
  stays usable over damaged older history.
- `boundary` is the head of every pallet at the moment trust was established: `enroll` takes
  every local pallet's, user and meta alike, the office excepted, and the remote's; a
  re-genesis takes every pallet's, the old office included. Parcels
  reachable from these are the pre-trust (legacy) history and may be unsigned;
  everything else must be signed. The boundary is exact ancestry — timestamps have
  second granularity and can be forged, so they never decide a security question.
- The trust file is a **one-way door**. Nothing removes it, and there is no "temporarily
  disable signing" escape hatch. A chain whose keys are all lost, or that has no usable
  admin left, is reset with `office regenesis`, which replaces the anchor and keeps the old
  office chain as attested history (see the trust guide, "Recovery: re-genesis").

## Threat model
What local trust does and does not defend against, by attacker capability. This is the model
for parcels written under trust. A warehouse that never enrolled detects only damage: a hash
exposes a byte changed inside an object, but anyone with write access can replace history with
a self-consistent rewrite, and anyone who can lift can write parcels and cells under any
operator's name. In a trusted warehouse the unsigned history from before the boundary is fixed
by the anchor's `boundary` while that anchor stands, so it cannot be rewritten or added to, but
the operator names in it were never checked. A re-genesis replaces the anchor and its boundary;
class 2 below covers one made by an attacker.

1. **No private keys, no warehouse write access** — cannot forge anything. Every object
   is content-addressed and every parcel is signed: modifying a byte changes the hash,
   the old signature no longer matches, and a new signature cannot be minted without an
   active key. `audit` fails loudly.

2. **Warehouse write access, but no private keys** — cannot forge *within* the existing
   trust root, but can replace the root wholesale (**re-genesis**): generate their own
   keypair, build a new self-signed genesis enrolling whoever they claim to be, rewrite
   the office ref and the trust file, and re-sign a forged history with their own key.
   `audit` passes on that machine, because it verifies against the local anchor and the
   anchor was replaced. The defense is not local: the genesis hash is a tiny, secret-free
   fact that belongs *outside* the attacker's reach — any other clone, a note, and (from
   Phase 3 on) the hosting service pin it. One comparison exposes the substitution, and
   content addressing makes the mismatch total: a rewritten history shares no hashes with
   the real one. The anchor's strength comes from being witnessed, not hidden — the same
   shape as certificate-transparency roots.

3. **Owns the machine** — out of scope. They have `~/.forklift-keys` (mode 0600, and
   unencrypted at rest unless the key is passphrase-protected, which a live attacker can
   capture as it is typed) and therefore the operator's identity itself; no VCS-level
   mechanism survives endpoint compromise. This is the boundary of the design, not a
   weakness of it: Phase 2's goal is *portable verifiability* — any copy of the
   warehouse plus the genesis hash can be verified offline, on hardware the attacker
   does not control.

A private key can be passphrase-encrypted at rest (`--passphrase` on `enroll`, `keygen` and
`rotate`): that protects leaked backups, stolen disks and an unattended process without the
passphrase, not a live attacker on the machine.

## Key lifecycle rules
- **enroll** — genesis only; later users are admitted by an enrolled operator.
- **admit** — the newcomer runs `profile keygen` locally and hands over the *public* key;
  an enrolled operator records user + key, signed with their own active key.
- **rotate** — issues a fresh key and retires the operator's active ones in one office
  parcel, signed with the **old** key: the continuity proof.
- **retire** — an operator can retire their own keys, and an admin can retire anyone's
  (compromise recovery). The office parcel is never signed with the key being retired,
  and an operator cannot retire their own last active key (that would be a self-lockout;
  rotate instead).

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
`history @office` and `audit @office` read it, `palletize --all` lists it. Working-pallet
operations (`palletize`, `shift`, `consolidate`, `stack`) only ever see the user
namespace, so they never touch it; the content guard (a tree carrying the
`.forklift/tracked/…` namespace is never materialized) stays as defense-in-depth.

## The manifest pallet (FORK-9 post-metadata)
The **manifest** is a second tracked-metadata meta pallet (`@manifest`, at
`.forklift/meta/manifest`) holding *post-metadata*: signed statements attached to a
parcel after the fact — approvals, review notes, and (later, §7.2) machine-authorship
provenance. An entry **references** its subject parcel and never mutates it (the §4.4
immutability invariant). Because a manifest parcel is signed by a tracked key, it
verifies through the ordinary pallet-history audit — no office-chain machinery and no
per-pallet server special-case (`audit @manifest`, `history @manifest`).

Unlike the office (full-state snapshots), the manifest is an **append-only DAG of
single-entry parcels**, and **authorship is the parcel's signature, not a stored field**.
Each `manifest note/approve` stacks one parcel carrying one entry, signed by its author;
the author *is* whoever signed it, so there is nothing to forge — a writer cannot record
an entry attributed to someone else, because that would require the other operator's key.
Reading collects every entry *reachable* from the head, which makes merging two diverged
manifests a plain **two-parent join parcel**: the union of independent records, never a
conflict. (The office cannot merge this way — its records interdepend — so it stays
linear; a diverged office is reconciled by hand.)

## The reserved tree namespace
Office parcel trees place the records under:

```
.forklift/tracked/users/<blake3(identifier)>.toml
.forklift/tracked/keys/<key-id>.toml
```

A manifest parcel carries its single entry in a sibling subtree (the id is
content-derived); a merge (join) parcel has an empty tree and no entry:

```
.forklift/tracked/manifest/<entry-id>.toml
```

The namespace is collision-proof by construction: the `.forklift` folder is the
warehouse root and is never tracked by `load`, so no user file can ever occupy these
paths. Trees carrying a top-level `.forklift` entry are refused by every operation that
materializes or merges trees.

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
keys; **reader** moves nothing (key self-service still applies). A record without a
`role` predates privileges and reads as admin — exactly the pre-privilege behavior.

**"May move any pallet" (and the `writer`/`reader` transport rules above it) is a transport
rule a server enforces only for a caller it resolves to that operator's own identity** — a
per-operator token, or an `authentication` hook — never a property of the signature itself.
A validly-signed, non-revoked parcel on an ordinary pallet passes `audit`/`verify_pallet_history`
regardless of its signer's role or `pallets` grant; those are checked nowhere in content audit
(`may_write_pallet` has exactly one call site in the workspace outside its own unit test, the
server's transport gate). A server run with only the shared static token, an `--open` server, or
the AWS serverless head does not resolve a caller to an operator identity at all, so none of
them enforce this table.

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

A key record (`key-id` = the Blake3 hex hash of the raw Ed25519 public key bytes):
```toml
key_id = "68fdb07d…"
operator = "mate@lonic.net"
public_key = "9d61b19d…"   # Ed25519, 32 bytes, hex
issued_at = 1782205234
retired_at = 1782206000    # absent while the key is active
```

Retired keys are **retained forever**: they still verify the parcels that were signed
while they were active. The private halves never enter the warehouse; they live in the
operator's `~/.forklift-keys/<key-id>.key` (mode 0600, `FORKLIFT_KEYS_DIR` overrides
the location).

A manifest entry (`entry-id` = `Blake3` of the fields below). Note there is no author
field — the author is the operator whose key signed the parcel, resolved on read, so it
cannot be forged:
```toml
subject = "0f1877dd…"    # the parcel this entry is about (never mutated)
kind = "approval"        # "approval" | "note" | "provenance"
recorded_at = 1782205234 # display only — validity comes from the signature, not time
body = "LGTM"            # the message (may be empty, e.g. a bare approval)
```

A **provenance** entry (§7.2) adds machine-authorship fields — which model produced the
parcel, with which tool, in which session. Signed like any entry, so paired with an
agent-class signer (§7.1) it is forge-proof evidence of *how* a change was made:
```toml
subject = "0f1877dd…"
kind = "provenance"
recorded_at = 1782205234
body = "generated the auth module"  # optional summary
model = "claude-opus-4-8"           # required — the compliance-critical field
tool = "claude-code"                # optional
session = "sess-42"                 # optional
transcript = "b3sum:…"              # optional — a prompt/transcript fingerprint
```

A **delivery** entry (§7.3) records that the subject parcel is a clean squash of a draft
pallet's checkpoint trail (`deliver`). The subject is the delivered parcel; the entry
references the kept trail so it stays discoverable without polluting the target's history:
```toml
subject = "f2998944…"    # the delivered (squashed) parcel
kind = "delivery"
recorded_at = 1782205234
body = "add the feature" # the delivered parcel's message
source = "draft/feature" # the draft pallet the trail came from (kept)
trail_head = "e6ee0a58…" # the trail tip — `history` on it walks the full trail
checkpoints = 3          # how many checkpoints were squashed
```

## The trust anchor (`.forklift/trust`)
Trust is established by the first `office enroll`: the genesis office parcel introduces
the first user and key and is self-signed by that key (trust-on-first-use). The trust
file records it:

```toml
genesis = "<genesis office parcel hash>"
enabled_at = 1782205234
boundary = ["<pallet head hash>", …]
```

- `genesis` anchors the office chain; `audit` refuses an office history that does not
  reach it.
- `boundary` is the head of every pallet at the moment trust was established: parcels
  reachable from these are the pre-trust (legacy) history and may be unsigned;
  everything else must be signed. The boundary is exact ancestry — timestamps have
  second granularity and can be forged, so they never decide a security question.
- The trust file is a **one-way door**. Nothing removes it, and there is no "temporarily
  disable signing" escape hatch; losing every active key means archiving the warehouse
  and re-establishing trust (re-genesis).

## Threat model
What local trust does and does not defend against, by attacker capability:

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

3. **Owns the machine** — out of scope. They have `~/.forklift-keys` (mode 0600, but
   unencrypted at rest) and therefore the operator's identity itself; no VCS-level
   mechanism survives endpoint compromise. This is the boundary of the design, not a
   weakness of it: Phase 2's goal is *portable verifiability* — any copy of the
   warehouse plus the genesis hash can be verified offline, on hardware the attacker
   does not control.

Possible future hardening (deliberately not Phase 2 scope): passphrase-encrypting
private keys at rest (the SSH precedent) — protects leaked backups and stolen disks,
not a live attacker on the machine.

## Key lifecycle rules
- **enroll** — genesis only; later users are admitted by an enrolled operator.
- **admit** — the newcomer runs `office keygen` locally and hands over the *public* key;
  an enrolled operator records user + key, signed with their own active key.
- **rotate** — issues a fresh key and retires the operator's active ones in one office
  parcel, signed with the **old** key: the continuity proof.
- **retire** — any enrolled operator can retire a key (compromise recovery). The office
  parcel is never signed with the key being retired, and an operator cannot retire
  their own last active key (that would be a self-lockout; rotate instead).

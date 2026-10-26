# Trust and identity — user guide

Forklift can make a warehouse's entire history **tamper-evident**: once you
establish trust, every parcel is signed, and any clone can verify the whole
history — including who was added and when — offline, with no server to trust.
This guide covers the `office` command (users, keys, roles, agents, recovery),
passphrase-protected keys, and `audit`.

If you just want local version control, you can skip all of this — signing is
opt-in per warehouse. It matters when you collaborate, host on a server, or want
provable authorship (including AI-agent authorship).

Contents:
1. [Concepts](#1-concepts)
2. [Establishing trust](#2-establishing-trust)
3. [Adding people](#3-adding-people)
4. [Roles and grants](#4-roles-and-grants)
5. [Multiple devices](#5-multiple-devices)
6. [Passphrase-protected keys](#6-passphrase-protected-keys)
7. [Agents, bots, and services](#7-agents-bots-and-services)
8. [Rotating and revoking keys](#8-rotating-and-revoking-keys)
9. [Recovery: re-genesis](#9-recovery-re-genesis)
10. [Auditing](#10-auditing)
11. [Profiles and pseudonymity](#11-profiles-and-pseudonymity)

---

## 1. Concepts

- **Operator** — an identity the chain sees, named by an opaque **operator id**
  (a minted UUID by default, or any string you choose, or a provider-issued id).
  No display name or email ever goes on-chain; ids are pseudonymous.
- **Key** — an Ed25519 keypair. The public half is tracked in the office; the
  private half stays on your machine (`~/.forklift-keys`, mode `0600`, never in
  the warehouse). A key's id is the Blake3 hash of its public key.
- **The office** — a *meta pallet* (`@office`) that holds operators and keys as
  signed tracked metadata. It lives in its own namespace (`.forklift/meta/`), so it
  reserves no user pallet name; its parcel history *is* the audit trail. Working-pallet
  operations never touch it; `forklift history @office` reads it.
- **Identity root** — the office pins one key per operator as their root; every
  further key must chain to it via signed endorsements (or be authorized by an
  admin). This is what makes "the provider can't forge a key for you" hold.
- **The trust anchor** — a one-way door. Before it exists, parcels are unsigned.
  After `office enroll`, every parcel must be signed, forever. There is no "turn
  signing off" — a full lockout is handled by re-genesis (§9) or by archiving and
  re-preparing the warehouse.

---

## 2. Establishing trust

The operator who enrolls becomes the first admin (the genesis, a
trust-on-first-use anchor):

```sh
forklift office enroll
```

This generates your identity-root key, writes the genesis office parcel
(self-signed), and establishes the trust anchor. **From now on every parcel in
this warehouse must be signed** — this cannot be undone.

- If a remote is configured, its pallet heads are folded into the trust boundary
  (history the remote already has stays valid unsigned), so the remote must be
  reachable — or pass `--offline` if it's gone for good.
- The boundary is what `enroll` vouches for, so it is checked again at the moment
  the anchor is written. If another bay of the warehouse moves a pallet past it,
  or writes an office of its own meanwhile, `enroll` is refused and writes
  nothing; run it again.
- A gap in that boundary does not by itself make `audit` refuse: a fully-signed
  pallet never asks the question, and an unsigned parcel still reachable from a
  boundary head this store holds is counted as legacy as usual. It is when an
  unsigned parcel is **not** reachable from anything present, **and** this store
  cannot resolve the boundary — it never held one of the named heads (`enroll`
  pins a remote's declared heads without fetching them, and `franchise` copies
  the anchor verbatim while fetching only the one pallet it franchises), or has
  genuinely lost one it once held, or an ancestor behind one — that `audit`
  refuses rather than concluding the parcel was stacked after
  trust: it names the specific boundary parcel it could not find and says
  plainly that it cannot tell whether the parcel predates trust or was stacked
  after it. Still fail-closed, still a non-zero exit, but not an accusation.
  A sync does not stop there: what it takes fetches the boundary parcel it lacks
  from the remote and is judged again (see *What a sync takes is verified*).
  The walk covers the whole boundary once per audit, not once per parcel, so
  the object it names is not a promise that it is the one a given parcel
  actually needed — supplying it does not guarantee resolution, since another
  gap may surface behind it on a rerun. Verify against a store with the full
  history for a definitive answer.
- Protect your key with a passphrase (recommended for a human): add
  `--passphrase` (see §6).
- That trust boundary keeps every parcel it names alive across `gc` and
  `compact` — even one no pallet points at any more — so the pre-trust history
  it attests can't be swept out from under a future audit.

### Joining a trusted warehouse: first contact

The first `franchise` or `lower` against a trusted remote **adopts its trust
anchor** — trust-on-first-use, like an SSH host key. It is never silent: the
command warns, naming the remote and the adopted genesis, and its report records
that genesis (`adopted_genesis` under `--json`, where the warning is a
`trust_adopted` document on stderr). Verify that genesis out of band — with
whoever runs the warehouse — because every later sync is checked against it.

To check it *before* anything is fetched, pin it:

```sh
forklift franchise <url> my-project --expect <genesis>
forklift lower --expect <genesis>
```

`--expect` is compared, exactly, against the genesis the remote declares in its
first response. A mismatch — or a remote with no anchor at all — is refused
before any download and before the target directory is created (`trust_changed`,
exit 22, `what_changed: "unexpected"`). The pin covers the genesis only: the rest
of the anchor (the trust boundary of pre-trust history) is adopted as the remote
declares it.

Once an anchor is held, a remote that later declares a different one is always
refused (`trust_changed`, exit 22). So is a remote that still holds an office
but declares no anchor: a server never lifts an office before its anchor and
never removes one, so the anchor was stripped (`what_changed: "removed"`;
nothing overrides it, and `lift` refuses it too rather than upload an anchor
over an office it never verified). A remote with neither is one trust was never established
on, and is not refused, so pin it with `--expect` to require an anchor. Under
`--json` the refusal carries
`details` — `subject` (the remote URL), `current` (the remote's genesis now),
`previous` (the one this warehouse holds) and `what_changed`: `regenesis` when
the new anchor names the held one as its prior (see §9), `unrelated` when it
does not (another warehouse, tampering, or a reset you never saw — nothing
overrides it).

### What a sync takes is verified

A head another store serves becomes this warehouse's at the ref write that
adopts it, so every such write is judged first, with the same check `audit` runs:
`lower` (of the working pallet or one named), `franchise`, `lift`'s auto-merge
of a remote that moved ahead, and `mirror` before it exports a head to git — and,
for a parcel already in the store, `palletize <name> <hash>` and `haul merge`.
With an anchor held, the head's history must verify against it and the office
this warehouse holds; nothing the remote serves gets past them, including a
remote stripped of both its anchor and its office. At first contact it must
verify against the anchor the remote serves, and the office the remote serves
must verify against that anchor too, before either is installed.

A head that does not verify is refused with `history_unverified` (exit 34) before
anything is recorded: no ref moves, no working file changes, and at first contact
no anchor, office or meta pallet is installed (a franchise reports it as
`franchise_incomplete` with `cause_code: "history_unverified"`, keeping what it
fetched; a mirror reports it as that pallet's failure). The objects fetched stay
in the store, unreferenced. Run `audit` where the history came from.

- The write that adopts a verified head lands only while the anchor and office
  it was judged against still stand, and an interrupted `lower` that `heal`
  finishes is held to the same pair: an office change in between rolls the
  lower back instead.
- A store that lacks the boundary ancestry a verdict needs — a sparse
  franchise, or a lower of a pallet whose pre-trust history is bounded by
  another pallet's head — fetches that boundary from the remote and judges
  again. A remote that cannot serve it is refused, naming the parcel.
- A merge signed here makes the merged history this warehouse's, so the `stack`
  that completes a conflicted merge judges the merged head again under the
  office it signs under: a key revoked since the merge began refuses it.
- A warehouse with no anchor, syncing with a remote that has none, has nothing
  to verify against, and takes what it is served.

---

## 3. Adding people

Forklift uses a certificate-signing-request (CSR) flow, so a private key never
leaves the machine that generated it and an admin can't enroll a key you don't
control.

**The newcomer** generates a key and prints their enrollment line:

```sh
forklift office keygen
# prints:  office admit <operator-id> <public-key> <proof-of-possession>
#          office link  <public-key> <proof-of-possession>
```

They hand you the `admit` line **over a channel where you can confirm it's really
them** (that human channel is the identity binding — crypto proves "same
key-holder", never "this person").

**An admin** admits them:

```sh
forklift office admit <operator-id> <public-key> <pop> --role writer
forklift office admit <operator-id> <public-key> <pop> --role writer --pallet main --pallet docs
```

The key becomes their identity root, endorsed by your admin key; their
proof-of-possession is verified, so a stolen line can't be re-attributed to
someone else. Office records are pseudonymous — no names or emails on-chain.

**Recovery — an admin re-keys an existing operator.** If someone lost every
device, they run `office keygen` on a new machine and hand you the line; you
authorize the new key for their existing identity:

```sh
forklift office authorize <operator-id> <public-key> <pop>
```

(Their own devices use `office link` instead — see §5.)

---

## 4. Roles and grants

Every operator has a role (recorded in their signed office record):

| Role | May do |
|------|--------|
| `admin` | Manage the office (admit, roles, others' keys) and move any pallet. |
| `writer` | Move working pallets (all, or a granted list); also **transport** any meta pallet (`@office`, `@manifest`, `@haul`, `@tags`, `@kv`) regardless of grants — content rules differ per pallet, see below. Manage their own keys. |
| `reader` | Move nothing; key self-service (rotation) still applies. |

**This table is a transport rule a server enforces for a caller it resolves to that
operator's own identity** — a per-operator token, or an `authentication` hook — not a
property of the signature itself. The `--pallet` grant is scoped to working pallets
only: a `writer`'s granted-pallet list restricts which *working* pallets they may move, but
every meta pallet is exempt from it **at transport** (the set is fixed — `@office`, `@manifest`, `@haul`, `@tags`, `@kv` — and an `@name` outside it is refused, by the server, the serverless head and a franchising client alike)
— any non-`reader` may move a meta pallet's ref regardless of what working pallets they are
granted. `docs/SERVER.md`'s "Per-operator tokens" section (row 3 and the paragraph below the
table) is the authoritative statement of that transport rule and of what the content-level
audit checks on top of it — in short, only `@office` gets a content-level signer-role check
there (the office chain walk, which every head, client and `audit` runs), and per-pallet grants are checked in no audit at all. `@kv`,
`@manifest`, `@tags` and `@haul` get a content check instead: each needs the office, and every value must
be stamped with the key that signed the parcel writing it, so no one can put a value in another
operator's name. On top of that, a `@manifest` entry can be changed or removed only by the
operator who wrote it; a `@tags` tag can be added only by an admin and never changed or
removed; and a haul is opened only in its opener's name, its reviews and comments are written
only by their own authors, and a merged haul is never reopened or closed. This holds on every head, the serverless one included, and a client checks it again
when it adopts or lowers those pallets — except the admin rule, which a client checks on what it
writes but not on history a server already admitted, so demoting a past tagger never makes the
tags they cut unreadable. A server run with only the shared static token, an `--open` server, or the
AWS serverless head does not resolve a caller to an operator identity at all, so none of them
run this transport gate — a `reader` who can reach the store directly (or hold that shared
credential) can sign and push an ordinary pallet's history like anyone else.

Change a role, or restrict a writer to specific pallets:

```sh
forklift office role <operator-id> reader
forklift office role <operator-id> writer --pallet main --pallet release
```

**No `--pallet` means unrestricted, not "none."** A writer's grant list is empty
by default, and an empty list means *every* working pallet — `--pallet` narrows
it. Re-running `office role <operator-id> writer` with no `--pallet` flags
clears any earlier restriction back to unrestricted, rather than leaving it
untouched.

The office always keeps at least one admin (lockout protection). Non-admins can
only change their own keys. Every office chain verification enforces that rule,
whether it runs on a remote at lift, in a client or in `audit`, so a parcel can
never grant its own author privileges.

---

## 5. Multiple devices

An identity can hold several keys (laptop, workstation, browser). On a new device
that shares your operator id, generate a key and link it with a key you already
hold:

```sh
# on the new device (configured with the same operator id):
forklift office keygen
# on a device that already holds one of your keys:
forklift office link <public-key> <pop>
```

`link` is a sigchain endorsement: the new key is trusted because a key you
already control signs "this is also me." Every warehouse that trusts your
identity then accepts it automatically.

---

## 6. Passphrase-protected keys

A private key on disk is unencrypted by default, and signing is non-interactive —
so **any process running as you can sign as you**, including an unattended AI
agent with shell access. To stop that, protect a human's key with a passphrase:

```sh
forklift office enroll --passphrase
forklift office keygen --passphrase
forklift office rotate --passphrase
```

The private key is then encrypted at rest (Argon2id → ChaCha20-Poly1305).
Signing prompts for the passphrase on the terminal; a decrypted key is held in
memory only for the duration of that one command. An unattended context with no
terminal **fails closed** — it cannot unlock the key, so it cannot sign as you.
`forklift office list` marks protected keys.

- **Automation escape hatch:** set `FORKLIFT_KEY_PASSPHRASE` to supply the
  passphrase non-interactively (CI, scripts, tests). This opts *out* of the
  interactive-only protection — use it only for identities you accept are
  passphraseless in practice, never to hand a human's passphrase to an agent.
- **The honest limit:** a passphrase stops an honest or semi-trusted unsandboxed
  agent, not a fully malicious one (which could replace the `forklift` binary or
  capture the passphrase as you type it). For that you need OS-level isolation (a
  separate user or sandbox) or a hardware/non-extractable key. The passphrase is
  the software boundary; isolation is the hard one.

**The principle:** automation signs *as itself*, never as a human. Human
identities get a per-action gate (passphrase); automated identities sign freely
under their own marked key (next section).

---

## 7. Agents, bots, and services

An automated principal is admitted like any operator but **marked** as one, so
"an agent wrote this, supervised by a human" is recorded in the signed office
record — forge-proof and offline-verifiable.

```sh
# the agent generates its own (passphraseless) key and hands you the line, then:
forklift office admit <id> <pub> <pop> --agent   --supervisor <human-operator-id>
forklift office admit <id> <pub> <pop> --bot
forklift office admit <id> <pub> <pop> --service
```

| Class | Meaning | Supervisor |
|-------|---------|------------|
| (default) human | A person | — |
| `--agent` | An AI agent bound to a supervising human | **required** |
| `--bot` | A scripted automation (dependency bumps, changelog, mirrors) | optional |
| `--service` | A build/CI/release identity | optional |

Rules, enforced at admission:
- An **agent requires a supervisor**, and the supervisor must be an enrolled
  **human** (automation cannot supervise automation).
- The class is *provenance*, orthogonal to the *role* (authority) — an agent can
  still be a writer scoped to certain pallets.
- The class is fixed at admission; `office role` never changes it.

`forklift office list` shows the class and supervisor; `--json` exposes both
fields. Automated identities should hold **passphraseless** keys — autonomy is
their nature — while humans hold passphrase-protected keys, so an agent uses its
own marked identity and can't sign as you. Decommission an agent with
`office retire`.

---

## 8. Rotating and revoking keys

```sh
forklift office rotate                    # issue a fresh key, retire the old ones
forklift office retire <key-id>           # revoke a key (routine)
forklift office retire <key-id> --compromised   # revoke a key that may be in other hands
```

- `rotate` signs the change with the **old** key (proving the rotation was
  authorized by its owner) and endorses the new key with it. Add `--passphrase`
  to protect the new key.
- Every revocation records a reason (retirement vs compromise) and a **distrust
  boundary**: the pallet heads you vouch for at that moment (the remote's
  included, unless `--offline`). Signatures by the revoked key are valid only on
  parcels reachable from that boundary — decided by **exact ancestry, never
  timestamps**, so a shifted clock can't forge validity. A `--compromised` key's
  signatures beyond the boundary fail every future audit.
- That boundary is checked again when the revocation is written: a pallet that
  another bay moved past it meanwhile refuses the `rotate` or `retire`, which
  then writes nothing; run it again.
- A parcel that a boundary head this store *does* hold already vouches for passes
  regardless of what else is missing — that question is asked first, and a yes is
  trustworthy however incomplete the store is. It is only for a parcel that is
  **not** vouched, in a store that cannot resolve the boundary — a head missing
  outright, or an ancestor behind one — that `audit` refuses (still fail-closed)
  and says so honestly: it names the specific boundary parcel it could not find
  and says it cannot tell whether the parcel predates the revocation or the key
  kept signing after it, rather than asserting tampering it cannot actually prove.
  The walk covers the whole boundary once per audit, not once per parcel, so
  the object it names is not a promise that it is the one this specific
  signature needed — supplying it does not guarantee resolution, since another
  gap may surface behind it on a rerun. Verify against a store with the full
  history for a definitive answer.
- Revocations are append-once for everyone (including admins) and a revoked key
  can no longer extend the office chain or endorse new keys.
- Like the anchor's own boundary, a distrust boundary keeps the history it
  names alive across `gc` and `compact`, so a parcel it vouches for can't be
  swept just because no pallet points at it any more. `gc` and `compact --all`
  both refuse outright — rather than sweep with an incomplete picture — if the
  office record behind these pins can't be read; `heal` (see [`cli.md`](cli.md))
  is more forgiving and will usually resolve it first.

---

## 9. Recovery: re-genesis

If a chain is fully locked — every key lost, no admin left — re-genesis resets
trust. It is recovery, not management: it's refused while you're an admin with a
usable key (use `rotate`/`link`/`admit`/`retire` for normal management).

```sh
forklift office regenesis            # dry-run: explains what would happen
forklift office regenesis --confirm  # actually reset
```

It creates a new self-endorsed trust root, stacks a parentless office genesis,
and replaces the anchor — pinning the old office head as **attested** history
(kept and readable, but its guarantee degrades from verified to attested). It is
deliberately **loud**: every clone refuses to sync until its holder consciously
re-accepts, and a remote accepts the reset only from the server operator's static
token.

The new anchor and the new office genesis are written together, with nothing able
to write a ref between them. If another bay of the warehouse moves the office, the
anchor or a pallet the reset vouches for while it runs, the reset is refused and
writes nothing; run it again. Accepting a reset is refused the same way when the
office or the anchor moved while it ran.

The attested history travels with the anchor. Accepting the reset, or franchising
from the remote after it, fetches the old office chain. Lifting a re-genesis to a
remote that has no anchor yet uploads that chain first. A server refuses an anchor
whose attested history it does not hold.

Every other clone's next sync is refused, naming the new genesis. On each, after
verifying out-of-band that the reset is legitimate **and** that the genesis is
the one the refusal named:

```sh
forklift office accept-regenesis                    # describes the change, accepts nothing
forklift office accept-regenesis --confirm <genesis> # the SSH host-key-change moment
```

`--confirm` takes the genesis itself, and it must equal the remote's genesis at
the moment of accepting. If the anchor changed again after it was shown — or the
value was mistyped — nothing is accepted, and the refusal names the current
value (`trust_changed`, `what_changed: "regenesis"`), so what you accept is
exactly what you verified. With nothing left to accept, `--confirm` succeeds only
for the genesis already held — any other value is refused, never reported as
accepted.

A remote whose anchor is malformed — a value that is not an object hash — is
refused by every command (`what_changed: "malformed"`); servers refuse to store
one in the first place.

---

## 10. Auditing

```sh
forklift audit          # office chain + the current pallet
forklift audit <pallet> # office chain + a specific pallet
forklift audit @office  # just the office chain
```

Everything is verified offline against the pinned anchor: the office chain
forward from genesis (each parcel signed by a key active in the previous state,
only genesis self-signs; and each within its signer's role in that state: a
non-admin changes only their own keys, and nobody removes or alters a key or
undoes a revocation), then the pallet's parcels (each signed by a tracked
key; pre-trust "legacy" parcels tolerated only when reachable from the recorded
boundary). Stripped or corrupted signatures, an unknown key, a parcel beyond its signer's
role, or a chain that doesn't reach genesis fail with a non-zero exit. The server head runs the exact
same checks before it accepts any ref update, so a remote can never be pushed
into a state a local `audit` would reject — and a client runs them again on
everything it takes (§2), so history altered in a remote's storage behind its
server is refused at the sync rather than found by a later `audit`.

---

## 11. Profiles and pseudonymity

The chain stores only operator ids and public keys — **zero PII**. A hosting
provider maps ids to display names behind its own policy, and that directory is the
only source of names Forklift shows — it keeps none of its own.

**Profiles** let one machine act as different operators in different warehouses —
a personal identity and one per organization:

```sh
forklift profile create work                            # mints an id
forklift profile create acme --id acme-issued-id        # or use a provider-issued id
forklift profile use work                                # this warehouse now acts as "work"
forklift profile list                                    # profiles and the local keys each holds
```

A warehouse selects a profile with `operator.profile`; the profile's id
then takes precedence over the plain `operator.*` values. The key directory keeps
an owner manifest so a machine can tell its identities' keys apart.

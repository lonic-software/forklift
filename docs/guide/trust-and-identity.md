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
- Protect your key with a passphrase (recommended for a human): add
  `--passphrase` (see §6).

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
| `writer` | Move working pallets (all, or a granted list) and manage their own keys. |
| `reader` | Move nothing; key self-service (rotation) still applies. |

Change a role, or restrict a writer to specific pallets:

```sh
forklift office role <operator-id> reader
forklift office role <operator-id> writer --pallet main --pallet release
```

The office always keeps at least one admin (lockout protection). Non-admins can
only change their own keys; a remote enforces the same rule on every lift, so a
parcel can never grant its own author privileges.

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
- If a store is missing one of the boundary's heads outright (a partial clone
  whose gap happens to matter for the parcel in question), `audit` never treats
  that as tampering — it refuses honestly, naming the specific boundary parcel it
  could not find. Fetch the full history, or audit a complete store, for a
  definitive answer.
- Revocations are append-once for everyone (including admins) and a revoked key
  can no longer extend the office chain or endorse new keys.

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

On another clone, after verifying out-of-band that the reset is legitimate:

```sh
forklift office accept-regenesis --confirm   # the SSH host-key-change moment
```

---

## 10. Auditing

```sh
forklift audit          # office chain + the current pallet
forklift audit <pallet> # office chain + a specific pallet
forklift audit @office  # just the office chain
```

Everything is verified offline against the pinned anchor: the office chain
forward from genesis (each parcel signed by a key active in the previous state;
only genesis self-signs), then the pallet's parcels (each signed by a tracked
key; pre-trust "legacy" parcels tolerated only when reachable from the recorded
boundary). Stripped or corrupted signatures, an unknown key, or a chain that
doesn't reach genesis fail with a non-zero exit. The server head runs the exact
same checks before it accepts any ref update, so a remote can never be pushed
into a state a local `audit` would reject.

---

## 11. Profiles and pseudonymity

The chain stores only operator ids and public keys — **zero PII**. A hosting
provider maps ids to display names behind its own policy; locally, your
`operator.name` is display-only and never leaves your machine.

**Profiles** let one machine act as different operators in different warehouses —
a personal identity and one per organization:

```sh
forklift profile create work --name "Work Me"           # mints an id
forklift profile create acme --id acme-issued-id        # or use a provider-issued id
forklift profile use work                                # this warehouse now acts as "work"
forklift profile list                                    # profiles and the local keys each holds
```

A warehouse selects a profile with `operator.profile`; the profile's id and name
then take precedence over the plain `operator.*` values. The key directory keeps
an owner manifest so a machine can tell its identities' keys apart.

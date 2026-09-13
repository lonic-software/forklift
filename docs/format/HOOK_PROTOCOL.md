# Hook protocol — version `2026-07-05`

The typed seam between a Forklift server head and a hosting provider (DESIGN.html
§8.13). Forklift owns these schemas; a provider (or any integration — a corporate
directory, a CI system, a billing service) implements the endpoints. The hosted
product is intended to be the reference implementation.

**The rule that makes hooks safe: they exist exactly where they are disjoint from
cryptographic verification.** A malicious or broken hook is bounded to *soft* damage
— a wrong display name, a bad access decision, a missed notification. It can never
forge signed content, alter a chain, or make an invalid parcel verify.

Never hookable, by design: signature verification, sigchain endorsement and
revocation verification, office-chain and pallet-history audit, content-authorship
authorization, and signing itself. Those are Forklift-owned, deterministic and
offline (`audit_utils`, `sign_utils`); no configuration reaches them.

Two axes to hold apart (§8.13): **authentication** — who is this credential (soft,
hookable) — versus **content authorization** — what this identity's keys may sign
(hard, chain-verified, never hookable). The authentication hook maps a credential to
an office identifier; every privilege still derives from that identifier's role in
the signed office metadata.

## The hooks

| Hook             | Direction      | Path | Failure policy    |
|------------------|----------------|------|-------------------|
| `authentication` | server → hook  | hot  | fail closed       |
| `admission`      | server → hook  | hot  | fail closed       |
| `event`          | server → hook  | cold | queue + retry     |
| `resolution`     | server → hook  | cold | show pseudonyms   |

**Every hook is invoked by the server head, never by the client.** The server holds
the hook URLs and secrets; a client only ever speaks the remote protocol to the
server. `resolution` is reached by the client through the server's `POST /v1/resolve`
endpoint (below) — this is what makes name-resolution policy *enforced* rather than
advisory: the server authenticates the caller before consulting the directory
(§8.12), which a client holding a shared hook secret could never do.

"Hot" hooks sit on the request path: a failure refuses the request (`503`), it never
waves it through. "Cold" hooks are side-band: a failure degrades output or delays a
notification, never a request.

## Transport & mutual authentication

Hooks are HTTP `POST` with a JSON body. **Every request is signed** — mutual
authentication is not optional, because a spoofable authentication hook is game
over. A hook endpoint MUST verify the signature (and the timestamp window) before
acting; a caller MUST refuse to configure a hook without a secret.

Request headers:

| Header                      | Value                                             |
|-----------------------------|---------------------------------------------------|
| `x-forklift-hook`           | the hook name (`authentication`, `admission`, `event`, `resolution`) |
| `x-forklift-hook-version`   | this protocol version (`2026-07-05`)              |
| `x-forklift-hook-timestamp` | Unix seconds, decimal                             |
| `x-forklift-hook-signature` | the request MAC, lowercase hex                    |

The MAC is a Blake3 keyed hash over `"<timestamp>" + "\n" + body`, keyed by
`blake3::derive_key("forklift hook protocol 2026-07-05 request mac", secret)`. The
key-derivation context is versioned with the protocol, so mixed versions can never
half-verify. Receivers refuse requests whose timestamp lies more than **300 seconds**
from their clock (replay window), then recompute and compare the MAC
(`hook_utils::verify_hook_request` is the reference implementation).

The response direction is authenticated by the channel: hook URLs SHOULD be HTTPS
in production (the server head sits behind a TLS-terminating proxy per
`docs/SERVER.md`; a hook endpoint is deployed the same way).

A receiver that does not recognize `x-forklift-hook-version` MUST refuse the
request; the version only changes when the wire format changes.

## `authentication` — credential → operator identifier (hot, fail closed)

Called by the server head for a bearer token it does not know locally (the static
token and the `--tokens` file are checked first; the hook extends them, it does not
replace them).

Request:

```json
{ "token": "<the presented bearer token, verbatim>" }
```

Response: `200` with the identity, or any non-`200` for "not a valid credential":

```json
{ "identifier": "<office operator identifier>" }
```

Semantics:

* The identifier is the pseudonymous operator id the office knows (§8.12). The hook
  **authenticates**; every privilege still derives from the office role of that
  identifier — a hook cannot grant content authority the chain does not.
* Fail closed: an unreachable hook or a malformed answer refuses the request with
  `503`; a non-`200` answer refuses it with `401`. Never `Open`.
* The server caches positive answers per token (default 60 seconds,
  `authentication_cache_secs`); a revoked credential outlives its revocation by at
  most the TTL. Negative answers are never cached.

## `admission` — soft policy gate (hot, fail closed)

Called before a mutating operation is admitted: quotas, plan limits, suspended
accounts, maintenance freezes. A denial is an access decision — it can never make
invalid content valid, and an approval can never make an unverifiable lift verify.

Request:

```json
{
  "action": "upload" | "ref_update" | "warehouse_create",
  "warehouse": "<warehouse id — multi-warehouse servers only>",
  "operator": "<office identifier of the acting principal, when it has one>",
  "pallet": "<the pallet a ref_update targets>"
}
```

Optional fields are absent (not `null`) when they do not apply; `operator` is absent
for the static token and open servers.

Response — `200` either way; a non-`200` or transport failure counts as a denial
(`503` to the client, fail closed):

```json
{ "allow": true }
{ "allow": false, "reason": "<shown to the refused client>" }
```

## `event` — webhooks (cold, queue + retry)

Fired after the fact, for side effects (notifications, indexing, billing counters,
mirrors). Delivery is at-least-once: the server head retries with backoff
(1 s · 5 s · 25 s · 125 s, five attempts) and logs a dropped event after the last —
consumers MUST tolerate duplicates and MUST NOT treat the event stream as a source
of truth (the warehouse is; the stream is a hint to go look). Any `2xx`
acknowledges; the response body is ignored.

```json
{
  "event": "pallet_updated" | "key_revoked" | "trust_established" | "trust_reset" | "warehouse_created",
  "warehouse": "<warehouse id — multi-warehouse servers only>",
  "operator": "<acting/affected office identifier, when known>",
  "pallet": "<the moved pallet — pallet_updated>",
  "old_head": "<head before the move; absent when the pallet was unborn>",
  "new_head": "<head after the move>",
  "key_id": "<the revoked key — key_revoked>",
  "detail": "<revocation reason (retirement/compromise), or the genesis hash of trust_* events>"
}
```

* `pallet_updated` — a ref update was accepted (a lift; consolidations arrive the
  same way). Office lifts fire it too, with `pallet` = the office pallet.
* `key_revoked` — an accepted office lift revoked a key (`operator` is the key's
  owner). One event per newly revoked key.
* `trust_established` / `trust_reset` — the trust anchor was set / replaced by a
  re-genesis (`detail` = the new genesis hash). A reset is precisely the loud
  moment §8.7 wants observers notified about.
* `warehouse_created` — `PUT /warehouses/{id}` created a warehouse (multi mode).

## `resolution` — operator identifiers → display names (cold, show pseudonyms)

Chains store zero PII (§8.12), so names exist only at display time, resolved through
a **server-mediated, policy-gated** service — never bundled with the warehouse, or
the policy would be advisory. The client reaches it through the server's
`POST /v1/resolve` endpoint (`docs/format/REMOTE_PROTOCOL.md`); the server, having
authenticated the caller, invokes this hook. The CLI resolves from its display paths
(`history`, `office list`), batched and bounded by the office roster.

Request (from the server to the directory):

```json
{
  "caller": "<the authenticated caller's operator id, when it has one>",
  "identifiers": ["<operator id>", "…"]
}
```

Response:

```json
{ "names": { "<operator id>": "<display name>", "…": "…" } }
```

`caller` is present so a policy-aware directory can tier its answer (§8.12: guests
resolve nothing, members only the peers of warehouses they share, admins everyone);
a dumb directory may ignore it and let the server pre-filter. Identifiers the caller
may not resolve, or that the directory does not know, are simply **absent** —
withholding is indistinguishable from not knowing, by design. Every failure
(unreachable, non-`200`, malformed) degrades to the pseudonymous identifiers on
screen; resolution can never fail a command, and its answers are never a
verification input.

## Configuration

**Server head** (`forklift-server serve --config …`, config-file-only — the hooks
come in URL+secret pairs):

```toml
[hooks]
authentication_url = "https://provider.example/hooks/auth"
authentication_secret = "…"
admission_url = "https://provider.example/hooks/admission"
admission_secret = "…"
events_url = "https://provider.example/hooks/events"
events_secret = "…"
resolution_url = "https://provider.example/hooks/resolve"
resolution_secret = "…"
authentication_cache_secs = 60   # optional
```

All four hooks are configured the same way, on the server. Each is independent;
configure any subset. A URL without a secret (or the reverse) is a startup error.

Only the `authentication` hook counts as configuring authentication (see docs/SERVER.md,
"Authentication"): a server whose `[hooks]` table has `admission`, `events`, and/or
`resolution` but no `authentication_url`, and no `--token`/`--tokens` either, still has *no
authentication configured* and refuses to start unless `--open` (or `open = true`) is also set.

**Client**: nothing to configure. A client resolves names by asking its configured
remote (`POST /v1/resolve`); with no remote, or a server without a resolution hook,
it simply shows pseudonyms.

## Conformance notes for implementers

* Verify the MAC and the timestamp window before touching the body.
* Answer `authentication` and `admission` fast; they sit on the request path (the
  server head's outbound timeout is 10 seconds).
* Keep the directory behind `resolution` dumb (`id → name`); enforce resolution
  *policy* at the hook (or a dedicated policy layer), so the hook can front an
  existing corporate directory (§8.13).
* Duplicate `event` deliveries are normal; idempotence is the consumer's job.

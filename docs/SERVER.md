# Running forklift-server

The self-hostable server head. It speaks `docs/format/REMOTE_PROTOCOL.md` and runs the
same storage and audit code the CLI runs locally — a remote can never be pushed into a
state a local `audit` would reject.

## Install

```sh
# macOS / Linux / Git Bash — installs the `forklift-server` binary to ~/.local/bin
curl -fsSL https://raw.githubusercontent.com/lonic-software/forklift/main/install.sh | sh -s -- server
```

On Windows, `$env:FORKLIFT_COMPONENT="server"; irm https://raw.githubusercontent.com/lonic-software/forklift/main/install.ps1 | iex`. From source: `cargo install --path crates/forklift-server`.

## Docker

A `Dockerfile` in the repo root builds a small (`debian:bookworm-slim`, non-root) image that
serves every prepared warehouse under `/data`:

```sh
docker build -t forklift-server .
docker run -d -p 9418:9418 -v forklift-data:/data forklift-server \
    serve --warehouses /data --addr 0.0.0.0:9418 --token <admin-secret>
```

The image's baked-in default command is `serve --warehouses /data --addr 0.0.0.0:9418` — with
no token, that command **refuses to start** (see "Authentication" below), so you always
override it: as above to set a token, with `--open` for a throwaway/local container, or to
serve a single warehouse (`serve --root /data/wh --token <secret>`). Create warehouses against
the running container with the admin token:

```sh
curl -X PUT -H "Authorization: Bearer <admin-secret>" http://localhost:9418/warehouses/<id>
```

Notes: mount `/data` as a **named volume** (or a host dir owned by uid `10001`) so the
unprivileged server can write it. Terminate TLS at a proxy in front of the container (below).
To **upgrade**, pull/build a new image and restart the container — the same "redeploy, don't
self-mutate" rule as a bare install.

## Quick start

```sh
forklift-server prepare --root /srv/forklift/wh
forklift-server serve --root /srv/forklift/wh --addr 127.0.0.1:9418 --token <secret>
```

Clients configure `forklift config remote.url http://…` (plus `remote.token`), or just
`forklift franchise <url> <dir> --token <secret>`.

## Serving many warehouses

```sh
forklift-server serve --warehouses /srv/forklift --token <admin-secret>
```

Every prepared subdirectory is served at `/warehouses/<id>/v1/…`; the id simply travels
inside `remote.url`. Warehouses are created explicitly — never as a side effect of a
lift:

```sh
curl -X PUT -H "Authorization: Bearer <admin-secret>" http://…/warehouses/<id>
```

Creation requires the static token; an open server refuses it (`403`).

## Authentication

**Some authentication is required to start the server at all.** Configure at least one of a
static `--token`/`token`, a `--tokens`/`tokens` operator-token file, or an `authentication` hook
(below) — or the server refuses to start, naming the missing piece. This is not "auth defaults
to a static token"; there is no default at all, on purpose.

To run with **no authentication** — local development, a LocalStack/CI throwaway, an internal
network you fully trust — pass `--open` (or `open = true` in the config file) explicitly:

```sh
forklift-server serve --root /srv/forklift/wh --addr 127.0.0.1:9418 --open
```

Every request is then served as a fully-privileged principal. There is no partial or inferred
form of this: omitting every auth setting *without* `--open` is a startup error, not an open
server — a forgotten token must fail loud, never silently serve the world.

## Configuration

Flags, or a TOML file (`--config server.toml`; flags override the file):

```toml
root = "/srv/forklift/wh"        # or: warehouses = "/srv/forklift"
addr = "127.0.0.1:9418"
token = "<secret>"               # static token: full access, gates creation
tokens = "/etc/forklift/tokens.toml"  # per-operator tokens (below)
max_body_mb = 4096               # refuse larger request bodies (default: 64 MiB, the largest
                                  # legitimate object after chunking — never unlimited)
rebuild_after_lifts = 20         # rebuild the bundle in the background (default: never)
open = false                     # explicit opt-out of authentication (default: false — see
                                  # "Authentication" above; a config with none of token, tokens,
                                  # or an authentication hook, and this unset, refuses to start)
```

Every key here is validated strictly: a key set to a value of the wrong type (e.g. an
unquoted `token = 12345`) is a startup error naming the file and the key, never silently
treated as unset — and an unrecognized key name (a typo) is refused the same way.

## Per-operator tokens (FORK-10)

The token file maps transport secrets to office identifiers — tokens are server-side
only and never enter the tracked metadata:

```toml
[operators]
"<token>" = "mate@lonic.net"
```

What an operator may do derives from their **role** in the target warehouse's office
(admin / writer / reader, plus per-pallet grants) — see
`docs/format/TRACKED_METADATA.md`. Roles are managed with `forklift office admit
--role …` and `forklift office role …`.

**Per-pallet grants apply only to a request that resolves to an office operator identity** —
one authenticated via this token file or an `authentication` hook, never to the shared static
`--token` or an `--open` server. Every row below is a distinct check `post_ref_update`
(`crates/forklift-server/src/server.rs`) runs against a ref-update request, in the order it
runs:

| # | Check | Function (file:line) | Runs when | What it verifies | What it does *not* cover |
|---|-------|----------------------|-----------|-------------------|---------------------------|
| 1 | Authentication | `check_auth`, `server.rs:868` | every request | resolves the request to a `Principal`: `Operator(id)` (a per-operator token or `authentication` hook), `Static` (the shared `--token`), or `Open` (no auth configured, `--open`) | nothing about what that principal may do |
| 2 | Admission hook | `check_admission`, `server.rs:1015`, called for a ref update at `server.rs:1981` | every principal, every ref update, only when `[hooks] admission_url` is configured (below) | a deployer-supplied soft-policy decision (quotas, plan limits, suspensions), given the pallet name — regardless of how the caller authenticated | not an office role/grant check; a no-op when unconfigured |
| 3 | Transport authorization | `server.rs:2029` (the block starting `server.rs:2020`; `user.may_write_pallet` itself is called only at `server.rs:2035`, the non-meta arm) | **only** for `Principal::Operator` that `office_user_of` resolves to `Some` — trust established (the anchor is set) and the office roster non-empty; an authenticated operator pushing to an untrusted warehouse, or during the bootstrap window before the office is lifted, clears this check with no test run at all (`office_user_of`'s two `None` arms, `server.rs:1181` and `server.rs:1191`) | for a **working** pallet: the operator's `role`/`pallets` grant permits moving *this* pallet ref (`may_write_pallet`); for a **meta** pallet (`@office`, `@manifest`, `@haul`, `@tags`): `may_write_pallet` is never consulted — only `role != Reader` is required, so a `writer` granted only some other pallet may still transport any meta pallet's ref | never runs for `Principal::Static` (the shared token) or an unauthenticated `--open` caller — they clear this check by never being subject to it; and for a meta pallet, never checks the `pallets` grant list at all, regardless of principal |
| 4 | Office chain authenticity | `verify_office_chain_memoized`, `server.rs:2109` (office update) / `server.rs:2154` (any other pallet, to obtain the office state) | only once the warehouse is trusted | every office parcel is signed by a key active in the office at the point it signed, chain reaches genesis | a non-office parcel's own signer's role — nothing below checks it: check 6 explicitly finds no role check in any of its three arms |
| 5 | Office privilege | `verify_office_privileges`, `server.rs:2116` | only for an office-pallet update, only once trusted | each office-modifying parcel is well-formed and authorized: signed by a key tracked at that point, with a parent, in a readable chain; its key-permanence obligations honored (a key is never removed or altered, and a revocation is never undone or added without a reason — binds admins too); and its *signer* held the office role (or self-service right) it needed as of that parcel's own signing (`error.rs:21-29`) | applies **only** to the office pallet's own chain — never to a working pallet's content or transport |
| 6 | Pallet history validity | `verify_pallet_history`, `server.rs:2159` | only for a non-office pallet, only once trusted | the same three-arm acceptance `docs/DEPLOYMENT.md`'s guarantee table documents for the AWS head's identical check (valid signature by a tracked, non-revoked key; or unsigned/untracked-key inside the trust boundary; or revoked-key inside that revocation's distrust boundary) | no role check and no per-pallet grant check, in any of the three arms |

The consequence: a caller authenticated with the static token is *not* resolved to
`Principal::Operator`, so row 3 never runs for it — absent an admission hook (row 2), it gets
uniform, full access to every pallet this server serves, subject only to rows 4–6. It is not,
however, equivalent to an unauthenticated `--open` server — it is strictly **more** privileged:
in multi-warehouse mode only the static token may create a warehouse at all (`put_warehouse`
refuses any principal but `Principal::Static`, so `--open` cannot create one — see "Serving many
warehouses" above). If you want per-pallet *transport* enforcement on a **working** pallet,
every caller that should be limited needs an operator token or hook identity — issuing the
static token to more than the server administrator defeats it, unless row 2's admission hook is
configured to compensate. That enforcement does not extend to meta pallets: row 3 never checks
the `pallets` grant list for `@office`, `@manifest`, `@haul`, or `@tags`, so an operator token
does not by itself restrict who may move those refs — only row 2's admission hook (refusing by
pallet name) reaches them.

Row 2 is the one mechanism that already restricts a static-token (or `--open`) caller per
pallet: configure `[hooks] admission_url` (below) to refuse by pallet name and it applies
regardless of how the caller authenticated. It is a soft-policy seam, not an office role/grant
check, but it is real per-pallet transport enforcement available today, not merely a gap. This
is the same shared-privilege property `docs/DEPLOYMENT.md` documents for the AWS serverless
head: that head's rows 4–6 (content-level, audit) still run on every push, but it has no
operator-identity mechanism and so has no equivalent of rows 2 or 3 — nothing shipped in this
repository adds one for that head either, short of a deployer-supplied API Gateway authorizer
(`docs/DEPLOYMENT.md`, "Auth at the gateway").

## Hooks (provider integration)

`docs/format/HOOK_PROTOCOL.md` — the typed seam a hosting provider (or any
integration) plugs into. Config-file-only, each hook independent:

```toml
[hooks]
authentication_url = "https://provider.example/hooks/auth"   # credential → identifier
authentication_secret = "…"                                  # signs every hook request
admission_url = "https://provider.example/hooks/admission"   # quota/suspension gate
admission_secret = "…"
events_url = "https://provider.example/hooks/events"         # lift/trust/revocation webhooks
events_secret = "…"
resolution_url = "https://provider.example/hooks/resolve"    # operator id → display name
resolution_secret = "…"
authentication_cache_secs = 60                               # optional, 0-86400 (24h max)
```

Every hook is invoked by the **server**, never the client — the server holds the URLs
and secrets, and each request carries a Blake3 keyed MAC (the endpoint must verify it —
see the spec). `authentication` and `admission` fail **closed**: an unreachable hook
refuses requests with `503`, it never becomes an open door. Events are delivered
at-least-once with backoff and logged when dropped. `resolution` powers
`POST /v1/resolve` (`history` / `office list` name display) — server-mediated so the
resolution policy is enforced, and best-effort so a failure just shows pseudonyms.
Verification (signatures, office chain, privileges) is never hookable — a hook can
refuse a request, it cannot make an invalid one verify.

`authentication_cache_secs` is a revocation-latency budget, not a general cache knob: it
bounds how long a credential the provider has already revoked can keep authenticating
before the server checks with the hook again. The server refuses to start with a value
over 24 hours (86400 seconds) — a revoked credential must not be able to outlive its
revocation by more than about a day.

## Operations

- **Health:** `GET /healthz` answers `200 ok`, unauthenticated — point the load
  balancer or systemd watchdog at it.
- **Logs:** structured request logs on stderr (`tracing`); `RUST_LOG` controls the
  filter (default `info`).
- **Bundles:** `forklift-server bundle --root …` builds the snapshot served at
  `/v1/bundles/latest` (fast franchising); `rebuild_after_lifts` automates it. The
  bundle is written atomically and streamed, so rebuilds never disturb serving.
  Successive versions of a file are **delta-compressed** (`docs/format/BUNDLE_FORMAT.md`,
  §9.1 #1), so the bundle moves each change rather than every whole file; the reconstructed
  objects are hash-verified on import, and older clients fall back to loose objects.
  Unlike `gc`, `bundle` is **safe to run against a live server** — it never deletes an object
  and writes atomically, so you can refresh a served root's bundle without downtime.
- **GC:** `forklift-server gc --root … [--grace-hours 24]` deletes objects no pallet
  head reaches. The grace period protects the objects of in-flight lifts. It is **refused
  while a server is serving that root** — it would sweep the server's in-flight objects and
  make a concurrent lift fail its ref update — so stop the server, gc, then restart (run it
  in a maintenance window, not against a live server). A hard-killed server leaves a
  `serve.lock` behind (a graceful SIGINT/SIGTERM removes it automatically); if `gc` reports
  the root locked by a process that is no longer running, remove
  `<root>/.forklift/serve.lock` and retry.
- **Shutdown:** SIGINT/SIGTERM drain in-flight requests before exiting.

## Updating

There is **no `self-update` for the server**, by design — a network service that rewrites its
own binary is an anti-pattern and an attack surface. A server is a deployed artifact, so you
**redeploy** it: fetch the new release and restart. The install script installs to a fixed
location and defaults to the latest release, so re-running it *is* the update — just stop the
service first:

```sh
systemctl stop forklift-server                                   # or however you run it
curl -fsSL https://raw.githubusercontent.com/lonic-software/forklift/main/install.sh | sh -s -- server
systemctl start forklift-server
```

Stop it first for two reasons:

1. A running process keeps executing the **old** binary until it restarts — replacing the file
   on disk does not hot-swap the live server.
2. The installer **refuses to overwrite a running `forklift-server`** (it detects one with
   `pgrep` and exits with an explanation), precisely so you don't unknowingly leave stale code
   running. Set `FORKLIFT_FORCE=1` to override (e.g. a blue-green host where you restart right
   after). The install is an atomic rename, so even a forced replace of a live binary is safe
   on Linux (no "text file busy").

Pin a version for controlled rollouts with `FORKLIFT_VERSION=v0.1.0`, or point
`FORKLIFT_BASE_URL` at a mirror for air-gapped hosts. The serverless (Lambda) head follows the
same principle — you ship a new function version rather than self-mutating.

## TLS and hardening

Terminate TLS at a reverse proxy — this is the supported deployment:

```
# Caddy: two lines, automatic certificates
forklift.example.com {
    reverse_proxy 127.0.0.1:9418
}
```

Request timeouts, connection limits and rate limiting also belong to the proxy layer
(nginx/caddy/haproxy do this better than any embedded knob). `max_body_mb` is the one
limit the server enforces itself, because it gates disk-fill abuse behind verification.

**The single-writer rule:** exactly one serving process per warehouse root. The ref CAS
mutex is in-process — do not point two processes (or two machines over NFS) at the same
root. Horizontal scale-out is what the AWS head's DynamoDB conditional writes are for
(DESIGN.html §4.6).

## systemd

```ini
[Unit]
Description=forklift-server
After=network.target

[Service]
User=forklift
ExecStart=/usr/local/bin/forklift-server serve --config /etc/forklift/server.toml
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

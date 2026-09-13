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
no token, that command **refuses to start** (see "Authentication" above), so you always
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
max_body_mb = 4096               # refuse larger request bodies (default: unlimited)
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
authentication_cache_secs = 60                               # optional
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

# Peer-to-peer over Tor

Work on the same warehouse with a few friends **without a hosted server, a fixed IP,
port-forwarding, or NAT configuration.** Each peer runs its own warehouse and, when it
wants to share, publishes it as a **Tor onion service** — a `.onion` address reachable from
anywhere on the Tor network. The others franchise (clone), lower (pull) and lift (push) to
that `.onion` exactly as they would to any remote; the client dials it through Tor
automatically.

This is the occasional-sync / seed-peer topology from
[the design rationale](../DESIGN.html) (§4.7): *a peer is just a warehouse that exposes the
[remote protocol](../format/REMOTE_PROTOCOL.md).* Forklift is a good fit for it because trust
does not depend on the transport — every parcel is Ed25519-signed and content-addressed with
Blake3, so a peer verifies what it receives no matter which `.onion` (or which anonymous
stranger) it came from.

> **Why Tor?** An onion service solves the three hard parts of ad-hoc peering in one existing,
> well-understood protocol: **reachability** (no port-forwarding — the service is reachable
> through the Tor network even behind NAT/CGNAT), **addressing** (the `.onion` *is* the
> address — no DNS, no dynamic-IP dance), and **transport security** (end-to-end encrypted and
> authenticated to that specific onion key). You share one string and you are done.

## Prerequisites

A running **`tor`** on each machine that will *connect* to a peer, and on each machine that
will *host* one. Install it from your package manager (`apt install tor`, `brew install tor`,
`pacman -S tor`, …) or the [Tor Project](https://www.torproject.org/). Started as a service it
listens, by default, on:

- **SOCKS** `127.0.0.1:9050` — how the Forklift *client* reaches a `.onion`.
- **Control** `127.0.0.1:9051` — how `forklift-server --tor` *publishes* a `.onion` (host side
  only; enable it in your `torrc`, see below).

Nothing here needs the Tor Browser — just the `tor` daemon.

## Share in one command: `forklift peer`

From inside the warehouse you want to share:

```sh
forklift peer
```

That's it. It publishes the warehouse as a Tor onion service and prints the one thing to hand a
peer:

```
  Your warehouse is live and shared over Tor — no server needed.

  Give your peer these two things:

    address   http://abcdefghijklmnop234567abcdefghijklmnop234567abcdefg.onion
    token     3f7c1e90-5b2a-4d18-9e6c-0a1b2c3d4e5f

  They clone it with:

    forklift franchise http://abcd…onion <dir> --token 3f7c1e90-…

  This address is saved — it stays the same next time you share.
  Peers can lift and lower while this runs. Press Ctrl-C to stop sharing.
```

- The **address is stable** — a key is kept under `.forklift/`, so the same warehouse keeps the
  same `.onion` every time you share. (`--ephemeral` gives a throwaway address instead.)
- The **token is minted once and reused** — pass `--token <t>` to set your own.
- Serving runs until **Ctrl-C**, which takes the address offline. Do your own Forklift work
  (`load`, `stack`, …) in another terminal meanwhile.

`forklift peer` runs the [`forklift-server`](../SERVER.md) head for you as a local child process
(the client and server stay separate binaries). It needs that binary installed —
`curl … install.sh | sh -s -- all` installs both, or point `--peer`'s `--server <path>` at it.
Options: `--token`, `--ephemeral`, `--server <path>`, `--tor-control <addr>`,
`--tor-control-password <pw>`.

## Under the hood: `forklift-server serve --tor`

`forklift peer` is a convenience wrapper; you can run the server head directly for more control
(serving many warehouses, per-operator tokens, a reverse proxy, systemd, …). Self-hosting a peer
for your own group is free under the head's licence — see [licensing](#licensing), below. Bind it
to **loopback** (Tor connects to it locally) and add `--tor`:

```sh
# In the warehouse you want to share:
forklift-server serve --root . --addr 127.0.0.1:0 --tor --token hunter2
```

`--addr 127.0.0.1:0` lets the OS pick a free local port; `--tor` publishes an onion in front of
it. On startup it prints the address to share:

```
forklift-server listening on http://127.0.0.1:54312
forklift-server onion service at http://abcdefghijklmnop234567abcdefghijklmnop234567abcdefg.onion
  a peer franchises it with: forklift franchise http://…onion <dir> --token <token>
```

Hand that `http://…onion` URL (and the token) to your friends over any channel. The onion lives
exactly as long as the server runs; stopping the server (Ctrl-C) tears the address down.

### A stable address across restarts

By default each run mints a **fresh** `.onion`. To keep **one address** your friends can save,
persist the key:

```sh
forklift-server serve --root . --addr 127.0.0.1:0 --tor \
    --tor-onion-key ~/.forklift/onion.key --token hunter2
```

The first run writes the onion's private key to that file (owner-readable only); every later run
re-offers it and reclaims the same address. **Guard that file** — whoever holds it can publish
your address.

### Tor control authentication

`--tor` talks to your local Tor over its **control port**. Enable it in `torrc` with one of:

```text
# torrc — cookie authentication (recommended; no secret to pass around)
ControlPort 9051
CookieAuthentication 1
```

The server reads the cookie file automatically (run it as a user allowed to read that file —
often Tor's group). Or use a password:

```text
# torrc — password authentication
ControlPort 9051
HashedControlPassword 16:…      # generate with: tor --hash-password 'your-pass'
```

```sh
forklift-server serve --root . --tor --tor-control-password 'your-pass' …
```

Flags: `--tor-control <addr>` (default `127.0.0.1:9051`), `--tor-control-password <pw>`,
`--tor-onion-port <n>` (the virtual port, default `80` so clients omit it), `--tor-onion-key
<path>`. All are also settable in the [server config file](../SERVER.md#configuration)
(`tor = true`, `tor_control = "…"`, `tor_onion_key = "…"`, …).

## Connect to a peer's onion

On the connecting side, just point Forklift at the `.onion`. The client detects a `.onion` host
and routes it through your local Tor SOCKS proxy automatically:

```sh
# Clone a peer's warehouse over Tor:
forklift franchise http://abcdefghij…onion myproject --token hunter2
cd myproject

# …work, stack parcels…

forklift lower      # pull their latest over Tor
forklift lift       # push yours over Tor
```

`franchise` records the `.onion` as this warehouse's `remote.url`, so subsequent `lift`/`lower`
need no extra flags.

### Tor client configuration

Two settings control the client transport (both optional):

| Key | Values | Default | Meaning |
|-----|--------|---------|---------|
| `remote.tor` | `auto` / `on` / `off` | `auto` | `auto`: use Tor only for `.onion` remotes. `on`: use Tor for **every** remote (reach a clearnet remote anonymously). `off`: never — even a `.onion` (for a custom transport). |
| `remote.torProxy` | a SOCKS URL | `socks5h://127.0.0.1:9050` | Where your local Tor listens. `socks5h` resolves the name at the proxy, which is required for `.onion`. |

```sh
forklift config remote.torProxy socks5h://127.0.0.1:9150   # e.g. Tor Browser's SOCKS port
forklift config --global remote.tor on                     # route everything through Tor
```

Because the default is `auto`, **a plain `http://` remote is completely unaffected** — nothing
changes for existing server-backed workflows; only a `.onion` remote is proxied.

## Working together without a central "main"

With no single hub, decide up front *whose* pallet is canonical. Two patterns work well:

- **A seed peer.** One person (or a spare always-on machine — this is the "seed node" idea from
  §4.7) hosts the shared onion and holds the authoritative pallets. Everyone lowers from it,
  works locally, and lifts back. Merges/consolidations land there. This keeps the familiar
  "one place is the truth" model without renting a server.
- **Round-robin.** Each peer hosts their own onion and others lower from it directly. You pull
  each other's work and `consolidate` (merge) as usual — Forklift's merge is origin-indifferent,
  so it does not matter which onion a parcel arrived through. Coordinate who integrates.

Either way, history stays verifiable end to end: run `forklift audit` any time to check the
signed chain offline, and `forklift blame` to see which signed operator (human or agent) wrote
each line — neither needs a server.

## Security notes

- **Keep the token.** Tor authenticates and encrypts the *transport* to the onion, but anyone who
  learns the `.onion` could still talk to it. `--token` gates writes and (if you enrol an office)
  per-operator roles; use it. Reads are open to anyone with the address unless you require a
  token.
- **Integrity comes from signatures, not the transport.** A franchise adopts the remote's trust
  anchor and verifies every object's hash and every parcel's signature on import, so a malicious
  relay or a wrong onion cannot forge history — it can at most refuse to serve.
- **Anonymity is Tor's.** The onion hides the host's IP; `remote.tor on` hides the client's. This
  is transport privacy, not a promise about what your peers do with the code.

## Troubleshooting

| Symptom | Likely cause / fix |
|---------|--------------------|
| `Could not reach the Tor control port at "127.0.0.1:9051"` | `tor` isn't running, or has no `ControlPort`. Add `ControlPort 9051` to `torrc` and restart it. |
| `Tor rejected cookie authentication` / `cookie file … is unreadable` | Run the server as a user allowed to read Tor's control-auth cookie, or switch to `--tor-control-password`. |
| Client hangs or fails to reach a `.onion` | Your local `tor` SOCKS isn't at `127.0.0.1:9050`. Set `remote.torProxy` to the right port. |
| `Error while configuring the Tor proxy` | The `remote.torProxy` value isn't a valid SOCKS URL (e.g. missing `socks5h://`). |
| The `.onion` changed after a restart | That's the default (ephemeral). Pass `--tor-onion-key <path>` for a stable address. |
| First connection is slow | Onion circuits take a few seconds to build; it settles after that. |
| `forklift peer` says it can't find the forklift-server binary | Install the server head (`install.sh server` / `all`), or point `forklift peer --server <path>` at it. |

## Licensing

The **client** (`forklift`) is MIT/Apache-2.0 — the Tor client transport is part of it, free to
use and build on. The **server head** (`forklift-server`) is source-available under the
[FSL-1.1](../../LICENSE-FSL): **self-hosting a peer for your own group is explicitly free** — the
only thing the licence withholds is running it *as a commercial hosting service that competes
with Forklift's own*. Peering with friends over Tor is squarely on the free side of that line.
See [LICENSING.md](../../LICENSING.md).

# Licensing

Forklift is **open-core**. The parts you run to use Forklift are open source; the
server heads you would run to *host Forklift as a service* are source-available under a
non-compete license that becomes fully open after two years.

## What is under which license

| Component | Crate / path | License |
|-----------|--------------|---------|
| Client library (object store, protocol client, formats) | `crates/forklift-core` | **MIT OR Apache-2.0** |
| Command-line client | `crates/forklift` | **MIT OR Apache-2.0** |
| Self-hostable server head | `crates/forklift-server` | **FSL-1.1-ALv2** |
| AWS serverless head | `crates/forklift-aws-lambda` | **FSL-1.1-ALv2** |
| Docs, specs, formats | `docs/` | **MIT OR Apache-2.0** |

- **[LICENSE-MIT](LICENSE-MIT)** and **[LICENSE-APACHE](LICENSE-APACHE)** — the client and everything else.
- **[LICENSE-FSL](LICENSE-FSL)** — the server heads only.

## What this means in practice

**The client is fully open source.** Use it, self-host with it, build proprietary tools on
top of it, sell those tools — the MIT/Apache terms permit all of it, with attribution.

**The server heads are source-available, not open source.** Under the
[Functional Source License 1.1](LICENSE-FSL) you may do anything with them — read, modify,
self-host, use internally, use for education/research — *except* one thing: you may not use
them to offer a **commercial product or service that competes with Forklift's own hosting**
(a "Competing Use"). Running the server for your own team, company, or projects is a
Permitted Purpose and is free. Reselling Forklift hosting is not.

**The restriction expires.** Every released version of the server heads is *also* granted
under the **Apache License 2.0 on the second anniversary of its release**. So no version is
ever permanently closed — the license only protects recent versions from commercial-hosting
competition, and everything becomes fully open on a rolling two-year delay.

## Why

Managed hosting is the intended business on top of this open project. Keeping the client
permissive maximizes adoption; reserving *commercial hosting* of the server heads is what keeps
that business viable. Self-hosting, internal use, and building on the open client stay free —
only reselling Forklift hosting requires a commercial arrangement. For commercial-hosting terms,
contact the maintainer.

## Contributing

Contributions to the **client** (`forklift-core`, `forklift`) are accepted under the same
dual MIT/Apache terms (inbound = outbound), as noted in the README.

Contributions to the **server heads** are under FSL-1.1. Because the maintainer reserves the
right to offer Forklift hosting commercially, server-head contributions require a signed
[Contributor License Agreement](CLA.md) granting the maintainer the rights to relicense and to
license commercially (you keep the copyright to your work). Signing is automatic — a bot
prompts on any pull request that touches the server heads; you register once per CLA
version, and each contribution you then submit carries its own grant. See
[CONTRIBUTING.md](CONTRIBUTING.md) for the full policy.

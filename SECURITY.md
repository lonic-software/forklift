# Security policy

## Reporting a vulnerability

Please report vulnerabilities privately. Do not open a public issue or pull request.

- **Preferred:** [open a private advisory](https://github.com/lonic-software/forklift/security/advisories/new)
  on GitHub.
- **Otherwise:** email admin@lonic.org.

Include the version (`forklift --version`), which part is affected (client, `forklift-server` or the
AWS head), and the steps or a warehouse that reproduces it.

We aim to acknowledge a report within 7 days and to tell you what we plan to do about it within 30.
Please give us a chance to ship a fix before you publish. We will credit you in the advisory unless
you ask us not to.

## Supported versions

Forklift is before 1.0. Fixes land on `main` and go out in the next release, so only the
[latest release](https://github.com/lonic-software/forklift/releases/latest) is supported.

## What counts

Forklift's selling point is signed, tamper-evident history, so these are the reports we most want:

- a forged or altered parcel, tag or office record that verification accepts;
- a remote, bundle or warehouse that makes the client write outside the working directory or use
  unbounded memory or disk;
- a server that lets a caller read or write beyond what their token and office privileges allow;
- history that reads back as something other than what was written, with no error.

The [threat model](docs/format/TRACKED_METADATA.md#threat-model) says what local trust does and does
not defend against, by attacker capability. Something it lists as undefended is not a vulnerability,
though we still want to hear if the document is wrong.

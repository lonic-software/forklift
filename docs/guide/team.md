# A team: from zero to working together

The [tutorial](tutorial.md) ends with one person sharing a warehouse through an `--open` server.
This page takes a team the rest of the way: a server that knows each person by a token, an office
that knows each person by a signing key, and a second person joining. It follows two people: Ada,
who starts the project and runs the server, and Bob, who joins it.

Each person works on their own machine, with their own keys and global settings, and each block
below says whose machine it runs on. The commands assume Ada's server listens on
`127.0.0.1:7171`, which is right if you try this alone with two user accounts on one computer.
Across machines, serve on an address the others can reach (see [Running a server](../SERVER.md))
and use it in place of `127.0.0.1:7171`.

As in the tutorial, each `$` line is a command and the lines under it are what forklift prints. A
`…` stands for text that differs on your machine. A word in angle brackets, such as `<genesis>`,
stands for a value forklift prints that a later command uses: copy it from your own output.

The test suite runs this page, as it runs the tutorial
(`crates/forklift/tests/integration/guide_pages.rs`), so the page cannot drift from the CLI.

## 1. Ada: a warehouse with an office

On Ada's machine, say who you are, once for every warehouse on this machine. The identifier is how
the office and your teammates will know you:

<!-- machine: ada -->

```console
$ forklift config set operator.identifier ada@example.com --global
```

Create the project and enroll in its office. Enrolling makes Ada its first admin and starts the
signed record of who may contribute; from then on every parcel must be signed:

```console
$ mkdir project
$ cd project
$ forklift prepare
Prepared warehouse.
$ forklift office enroll
Enrolled "ada@example.com" and established trust.
Genesis parcel:    <genesis>
Identity root key: …

From now on every parcel stacked in this warehouse must be signed. This cannot be undone (a lockout is handled by re-genesis, "forklift office regenesis").
$ echo 'Our project.' > readme.md
$ forklift load readme.md
$ forklift stack -m "Start the project"
Stacked parcel … on pallet "main".
```

The genesis parcel is the warehouse's identity. Note it: Bob will check it when he clones.

## 2. Ada: a server with a token per person

Prepare an empty warehouse for the server, beside the project:

```console
$ cd ..
$ forklift-server prepare --root server
Prepared warehouse at "server".
```

Write the token file. Each line maps a secret token to the identifier of the person who holds it.
Use long random tokens in practice, and keep the file private: anyone with a token can reach the
server as that person.

```console
$ echo '[operators]' > tokens.toml
$ echo '"ada-secret" = "ada@example.com"' >> tokens.toml
$ echo '"bob-secret" = "bob@example.com"' >> tokens.toml
```

The server reads this file only when it starts. Write a line for everyone you are about to invite
before you start it, and to add someone later, add their line and restart the server.

Open a **second terminal** in the same folder and start the server there. Leave it running:

```console
$ forklift-server serve --root server --addr 127.0.0.1:7171 --tokens tokens.toml
forklift-server listening on http://127.0.0.1:7171
```

## 3. Ada: lift the project

Back in the first terminal, point the project at the server with Ada's token, and lift. The first
lift also carries the office, so the server learns who may contribute:

```console
$ cd project
$ forklift config set remote.url http://127.0.0.1:7171
$ forklift config set remote.token ada-secret
$ forklift lift
Lifted the office pallet: 1 new parcel(s).
Lifted pallet "main" to …: 1 new parcel(s), … object(s) and 1 signature(s) uploaded.
```

Now send Bob three things: the server's address, his token (`bob-secret`), and the genesis from
step 1. Send the genesis by a channel you already trust, such as in person or a chat you know is
Ada's.

## 4. Bob: clone, and ask to be admitted

On Bob's machine, say who you are:

<!-- machine: bob -->

```console
$ forklift config set operator.identifier bob@example.com --global
```

Clone with your token. `--expect` checks that the server's genesis is the one Ada sent before
anything is fetched, so a server that is not Ada's is refused instead of trusted:

```console
$ forklift clone http://127.0.0.1:7171 project --token bob-secret --expect <genesis>
Warning: FIRST CONTACT with http://127.0.0.1:7171: adopted its trust anchor, genesis <genesis>. …
Adopted the remote's trust anchor (genesis <genesis>); every parcel is signed from now on.
Cloned http://127.0.0.1:7171 into "project": pallet "main" at … (… object(s) fetched loose).
$ cd project
```

The warning is printed on every first contact with a server. Here `--expect` has already checked
the genesis it names, so it is the one Ada sent.

Bob can read the project now, but not stack: the office does not know his key yet. Generate one.
The key stays on this machine; what it prints is the line to send Ada:

```console
$ forklift profile keygen
Generated a new keypair for the default identity (the global operator.identifier); the private key stays on this machine.
Key id:      …
Operator id: bob@example.com

To be admitted, hand an office admin this line:
  office admit bob@example.com <key> <proof>

To link this as another device of an identity already enrolled as bob@example.com, run
on a machine that holds one of its keys:
  office link <key> <proof>
```

Until Ada admits him, `stack` refuses and says that Bob is not enrolled.

## 5. Ada: admit Bob

On Ada's machine, run the line Bob sent, with `forklift` in front, and lift the office so the
server and everyone else learn of it:

<!-- machine: ada -->

```console
$ forklift office admit bob@example.com <key> <proof>
Admitted "bob@example.com" as writer with identity root … (office parcel …).
$ forklift lift
Lifted the office pallet: 1 new parcel(s).
Already up to date: the remote has pallet "main" at ….
```

A writer may stack and lift. Pass `--role reader` for someone who should only read, or
`--role admin` for someone who may admit others; `office role` changes a role later.

## 6. Bob: lower, then work

On Bob's machine, `lower` brings the updated office, and with it Bob's admission:

<!-- machine: bob -->

```console
$ forklift lower
Office pallet updated from the remote.
Already up to date: pallet "main" is at the remote head.
$ echo 'Notes from Bob.' > notes.md
$ forklift load notes.md
$ forklift stack -m "Add notes"
Stacked parcel … on pallet "main".
$ forklift lift
Lifted pallet "main" to …: 1 new parcel(s), … object(s) and 1 signature(s) uploaded.
```

## 7. Ada: lower, and check who did what

On Ada's machine, lower Bob's work and audit it. Every parcel is signed, and each signature names
its signer in the office:

<!-- machine: ada -->

```console
$ forklift lower
Lowered pallet "main" to … (… object(s) and 1 signature(s) fetched).
$ cat notes.md
Notes from Bob.
$ forklift audit
Office chain verified back to genesis <genesis>.
Pallet "main" verified: 2 signed parcel(s) valid.
$ forklift office list
ada@example.com — admin
  key … (active, on this machine, identity root)
bob@example.com — writer
  key … (active, identity root)
```

Stop the server with Ctrl-C when you are done.

## Tokens and signatures are separate

A token and a signing key answer different questions, and neither stands in for the other:

- The **token** lets a request through the server. The server looks up the person the token is
  mapped to and checks what their role in the office allows them to lift.
- The **signature** on each parcel says who made it. `audit` and every client verify it against the
  office, whoever lifted the parcel.

So the token a parcel arrived with says nothing about who wrote it: a parcel signed by Ada and
lifted with Bob's token is Ada's parcel. Give each person their own token so the server can apply
their role, and rely on the signatures, not the tokens, for who did what.

## Where to go next

- [Running a server](../SERVER.md): the token file, roles and per-pallet grants, and serving many
  warehouses.
- [Trust and identity](trust-and-identity.md): roles, more devices for one person, agents, revoking a
  key, and recovering from a lost one.
- [`office`](cli.md#7-identity-signing-and-agents) and
  [remotes](cli.md#6-working-with-remotes) in the command reference.

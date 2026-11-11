# You leaked a secret: rotate it, then redact it

A password, an API token or a private key went into a parcel, and the parcel was lifted. This page
says what to do, in order. **Rotating the secret comes first**, and it is the step that actually
protects you. Redacting it afterwards (`forklift redact`) removes the content from every warehouse
that hears of the redaction, so it stops spreading, but it cannot reach a copy that has already
left.

As in the [tutorial](tutorial.md), each `$` line is a command and the lines under it are what
forklift prints. A `…` stands for text that differs on your machine. A word in angle brackets, such
as `<leak>`, stands for a value forklift prints that a later command uses: copy it from your own
output. The test suite runs this page (`crates/forklift/tests/integration/guide_pages.rs`), so it
cannot drift from the CLI.

## 1. Rotate the secret first

Revoke the leaked secret where it was issued, and replace it: a new token from the service, a new
password, a new key pair. Do this before anything on this page, and treat the old secret as public
from the moment it was lifted. A redaction cannot do this part for you, because by the time you
notice the leak the content may already be outside Forklift's reach:

- in a teammate's warehouse, and in their working folder as a file, from a `clone` or `lower` made
  while the content was there;
- in a git repository written by `forklift git export`, or in the git mirror
  (`.forklift/mirror.git`) that `forklift git mirror` keeps;
- in a CI checkout, a backup of the server or of anyone's disk, an editor's history, a terminal's
  scrollback, or anywhere someone copied the file.

A redaction never touches any of these outside copies. Forklift deletes what its own warehouses
hold, and the rest is up to whoever holds it.

## 2. A project with a leaked token

To follow along, make a project whose first parcel carries a token in `settings.env`, lifted to a
server, and a teammate's copy cloned from it. With a project of your own, skip to section 3.

```console
$ forklift config set operator.identifier ada@example.com --global
$ forklift-server prepare --root server
Prepared warehouse at "server".
```

Open a **second terminal** in the same folder and start the server there. Leave it running. The
second line below is one the server prints later, in section 5, when it deletes the redacted
content:

```console
$ forklift-server serve --root server --addr 127.0.0.1:7171 --open
forklift-server listening on http://127.0.0.1:7171
…  INFO forklift_server::server: deleted redacted content objects=1 chunks=0 bytes_freed=37 held=0
```

Back in the first terminal, make the project and lift it, then clone a teammate's copy beside it:

```console
$ mkdir project
$ cd project
$ forklift prepare
Prepared warehouse.
$ echo 'Our project.' > readme.md
$ echo 'API_TOKEN=sk-live-1234' > settings.env
$ forklift load .
$ forklift stack -m "Start the project"
Stacked parcel <leak> on pallet "main".
$ forklift config set remote.url http://127.0.0.1:7171
$ forklift lift
Lifted pallet "main" to <leak>: 1 new parcel(s), … object(s) and 0 signature(s) uploaded.
$ cd ..
$ forklift clone http://127.0.0.1:7171 copy
Cloned http://127.0.0.1:7171 into "copy": pallet "main" at <leak> (… object(s) fetched loose).
$ cd project
```

The token is now on the server and in the teammate's copy.

## 3. Take the file out of every pallet head

A redaction removes content from history, never from a pallet's head, so first stack a parcel
without the file. Keep the file from coming back: `forklift remove` stages its removal and leaves it
on disk, where the next `forklift load .` would stage it again, so ignore it too. The rotated token
from section 1 can then live in `settings.env` without being tracked:

```console
$ forklift remove settings.env
$ echo 'settings.env' > .forkliftignore
$ forklift load .forkliftignore
$ forklift stack -m "Stop tracking settings.env"
Stacked parcel … on pallet "main".
```

Do the same on every pallet whose head has the file. If you miss one, the next step refuses as
`head_names_content` and names each pallet, its head and the path the file has there (see
[recovery](recovery.md#head_names_content-exit-50)).

## 4. Record the redaction

Name the file at a parcel that has it, as `<revision>:<path>`. Here that is the first parcel:

```console
$ forklift redact record <leak>:settings.env -m "leaked API token"
Recorded the redaction of object … (@redactions head …).
It is pending until the remote reports it applied.
```

A redaction is permanent: it is recorded on the `@redactions` meta pallet, it cannot be undone, and
from now on no pallet head may name that content again. In a warehouse with an office, only an
admin may record one. The reason you give with `-m` is kept on `@redactions` for good and copied to
every clone, so describe the leak there and never quote the secret.

## 5. Lift it, and apply it

The redaction waits until the remote has applied it, so lift it to the server:

```console
$ forklift lift
Lifted the @redactions pallet.
Lifted pallet "main" to …: 1 new parcel(s), … object(s) and 0 signature(s) uploaded.
```

The server applies the redaction once none of its pallet heads names the content: it deletes its
copies and from then on refuses the content. Its terminal says so (the line shown in section 2).
Now apply the redaction here too, which deletes this warehouse's copies:

```console
$ forklift redact apply
… leaked API token
    applied
Deleted 1 redacted object(s) and 0 chunk(s), freeing 37 B.
```

The history is unchanged, and in a warehouse with an office `forklift audit` still verifies it;
only the file's content is gone from it. A command
that needs that content, such as a `show`, `restore` or `diff` of the first parcel's
`settings.env`, now refuses as `content_redacted` (see
[recovery](recovery.md#content_redacted-exit-49)).

## 6. Every copy that hears of it

Each teammate's warehouse applies the redaction the next time it takes from the remote. A `lower`
in the teammate's copy deletes the content, along with `settings.env` in its working folder, since
the file is no longer tracked:

```console
$ cd ../copy
$ forklift lower
Updated the @redactions pallet from the remote.
Lowered pallet "main" to … (… object(s) and 0 signature(s) fetched).
Applied the redaction of …: the remote reports it applied.
Deleted 1 redacted object(s) and 0 chunk(s), freeing 37 B.
$ forklift redact list
… leaked API token
    applied
```

A new `clone` never fetches the content at all. A git mirror the warehouse keeps
(`.forklift/mirror.git`) may still hold a copy written before the redaction: forklift does not
delete it, and the command that applied the redaction names it. Delete or rewrite that mirror
yourself, along with any git repository an earlier `forklift git export` wrote.

## 7. What a redaction cannot remove

A redaction removes **a file's content** and nothing else. These stay in history for good, in every
copy, so a secret in one of them can only be rotated:

- **a stack message** or a parcel's other fields, and **a file or folder name**;
- **anything in a meta pallet's history**: a haul's title, description, reviews and comments; a
  tag's message; custom metadata (`@kv`); the office; the manifest; and the reasons on
  `@redactions` itself.

Meta-pallet history is never rewritten. When the file's content is also part of a meta pallet's
history, `forklift redact record` refuses as `redaction_blocked` and names the meta pallets (see
[recovery](recovery.md#redaction_blocked-exit-48)). Nothing was recorded, and the content can never
be removed from this warehouse or any copy of it.

Content that a server's host withholds, such as after a legal notice, is a different mechanism that
the host runs, not the warehouse's admins: see [recovery](recovery.md#content_withheld-exit-51).

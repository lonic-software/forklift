# Forklift guides

Forklift is an open-source version control system designed from day one for
serverless hosting. It is a complete, fast, parallel VCS you can use entirely
locally, and a remote protocol whose server side is storage plus thin verifiers
rather than a stateful pack-negotiating process.

If you know git, you already know the shape of forklift — it just wears a
warehouse theme, and it adds a few things git can't retrofit (tamper-evident
signed history, collaboration metadata that lives in the repo, and a first-class
machine/agent interface).

## Which guide do I want?

| You are… | Read |
|----------|------|
| A user learning the CLI | [`cli.md`](cli.md) — every command, by workflow, with examples |
| Setting up signing, users, or agents | [`trust-and-identity.md`](trust-and-identity.md) |
| Running a forklift server | [`../SERVER.md`](../SERVER.md) |
| Driving forklift from a script or AI agent | [`../MACHINE_INTERFACE.md`](../MACHINE_INTERFACE.md) |
| Contributing to forklift itself | [`contributing.md`](contributing.md) |
| Looking for the design rationale / roadmap | [`../DESIGN.html`](../DESIGN.html) (the source of truth) |
| Reading a byte-level format spec | [`../format/`](../format/) |

## The warehouse model in one minute

A forklift repository is a **warehouse**. Instead of git's commit/branch/index
vocabulary, forklift uses shipping-yard words — the concepts map one-to-one:

| Git | Forklift | What it is |
|-----|----------|------------|
| repository | **warehouse** | the project and all its history (a `.forklift/` folder) |
| commit | **parcel** | an immutable snapshot: a tree + parents + authorship + description |
| branch | **pallet** | a movable pointer to a head parcel |
| the index / staging area | the **inventory** (a.k.a. the **dock**) | what the next parcel will contain |
| `commit` | **stack** | record the inventory as a new parcel |
| `checkout <branch>` | **shift** | switch the working directory to a pallet |
| `checkout -b` / branch | **palletize** | create a new pallet |
| `merge` | **consolidate** | merge another pallet in |
| `stash` | **park** | set work aside and return to the head |
| `add` | **load** | stage a file or directory |
| `rm --cached` | **remove** | stage a removal |
| `restore --staged` | **unload** | unstage (undo a `load`) |
| `status` | **stocktake** | show staged and unstaged changes |
| `push` | **lift** | upload to a remote |
| `pull` | **lower** | fetch and fast-forward from a remote |
| `clone` | **franchise** | open a local copy of a remote warehouse |
| `init` | **prepare** | create a warehouse here |
| `log` | **history** | walk the parcel graph, newest first |
| a user | an **operator** | who authored a parcel (an opaque id) |
| — | the **office** | the signed registry of operators and their keys |

Two more terms you'll meet:

- A **blob** is a file's contents; a **tree** is a directory listing (sorted,
  deterministic — identical content always hashes identically). Parcels point at
  a root tree. Everything is content-addressed with Blake3 and immutable.
- A **revision** is anywhere a point in history is named: a pallet name *or* a
  parcel hash (a unique prefix, ≥ 4 hex characters). `history`, `diff`,
  `palletize` and `audit` all accept either.

## The shortest possible tour

```sh
forklift prepare                          # create a warehouse here
forklift config operator.name "Ada"       # who you are (optional; an id is minted otherwise)

echo "hello" > readme.txt
forklift load .                            # stage everything
forklift stack "first parcel"             # commit

forklift palletize feature                 # branch and switch to it
echo "more" >> readme.txt
forklift load . && forklift stack "work"

forklift shift main                        # back to main
forklift consolidate feature               # merge feature in
forklift history                           # see what happened
```

For anything deeper — remotes, signing, agents, servers — follow the table above.

## Keeping these guides current

These guides document the **shipped** surface. Areas still under active design
(the hosting-provider layer, the rest of the AI-agent track, file-level
privileges) are documented as they land; until then they live in `DESIGN.html`
as roadmap. When you change a command's flags, add a command, or change a
format, update the relevant guide in the same change — the same discipline the
`DESIGN.html` changelog already follows. See
[`contributing.md`](contributing.md#8-keeping-docs-in-sync).

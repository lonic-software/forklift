# Switching from git: moving a project to Forklift

`forklift git import` brings a git repository's history into a warehouse in one command: commits
become parcels, branches become pallets and tags come along. This page walks through the switch,
from an existing git repository to a team working on Forklift. It is honest about what the import
does not carry, so you can decide before you rely on it. The switch is not a trap:
[Leaving Forklift](leaving.md) takes the project back to git.

As in the [tutorial](tutorial.md), each `$` line is a command and the lines under it are what it
prints. A `…` stands for text that differs on your machine. The test suite runs this page
(`crates/forklift/tests/integration/guide_pages.rs`), so it cannot drift from the CLI. You need
`git` on your PATH.

## 1. A git repository to start from

To follow along, make a small git repository with two branches and a release tag. With a project
of your own, skip to section 2.

```console
$ mkdir project
$ cd project
$ git init -b main
Initialized empty Git repository in …
$ git config user.email ada@example.com
$ git config user.name Ada
$ echo 'Our project.' > readme.md
$ git add readme.md
$ git commit -m "Start the project"
[main (root-commit) …] Start the project
 1 file changed, 1 insertion(+)
 create mode 100644 readme.md
$ git tag -a v1.0 -m "First release"
$ git switch -c feature
Switched to a new branch 'feature'
$ echo 'A feature.' > feature.md
$ git add feature.md
$ git commit -m "Add the feature"
[feature …] Add the feature
 1 file changed, 1 insertion(+)
 create mode 100644 feature.md
$ git switch main
Switched to branch 'main'
$ cd ..
```

## 2. Import a clone, with a local branch for each one you keep

Import from a clone, not from the copy you work in, so your git work stays as it is until the
switch has proved itself. The import turns each **local** branch into a pallet, and a fresh clone
has only one local branch. Make a local branch for every other branch you want to keep:

```console
$ git clone project copy
Cloning into 'copy'...
done.
$ cd copy
$ git branch
* main
$ git branch feature origin/feature
branch 'feature' set up to track 'origin/feature'.
```

Tell Forklift who you are, make a warehouse in the same folder and import:

```console
$ forklift config set operator.identifier ada@example.com --global
$ forklift prepare
Prepared warehouse.
$ forklift git import .
Imported 2 commit(s) into 2 pallet(s) (2 tree(s), 2 blob(s)).
Checked out "main".
Imported 1 tag(s).
Packed the imported store: 6 object(s) into 1 pack(s), 1 delta-compressed.
Tip: "forklift store compact --all --redelta" can significantly shrink this store (one-shot, takes a few minutes on large histories).
The history is unsigned (imported). Run "forklift office enroll" to establish trust — it anchors the imported history as the legacy boundary.
$ forklift pallet
  feature
* main
$ forklift tag list
v1.0  …  by ada@example.com (unsigned)  First release
$ forklift stocktake
On pallet "main" (head …)

The inventory matches the pallet head; nothing is staged.

The working directory matches the inventory.
```

The import writes the history straight into packed storage, so a large repository arrives compact.
The tip it prints is optional: one `store compact --all --redelta` can shrink the store further.
A long history takes a while: the Linux kernel's takes minutes.

Git and Forklift now share the folder. `.forklift/` ignores itself, so git does not list it, and
Forklift always ignores `.git`, so both read clean. Git rewrites some files as it checks them out: line endings under
`core.autocrlf`, filters such as Git LFS. Such a file does not hold the content git stored, so the
import warns, naming it, and `stocktake` shows it as modified. Load and stack it as it stands, or
`forklift restore --confirm` it to write the stored content back. [The CLI
guide](cli.md#git-import--migrate-a-git-repository) says more.

## 3. Sign from here on

Imported history is unsigned, as it was in git. Enroll once, straight after the import, and every
parcel from then on is signed. The imported parcels stay, as the legacy history that came before
trust:

```console
$ forklift office enroll
Enrolled "ada@example.com" and established trust.
Genesis parcel:    …
Identity root key: …

From now on every parcel stacked in this warehouse must be signed. This cannot be undone (a lockout is handled by re-genesis, "forklift office regenesis").
$ forklift audit
Office chain verified back to genesis ….
Pallet "main" verified: 0 signed parcel(s) valid, 1 legacy parcel(s) predate trust and are unsigned.
```

[Trust and identity](trust-and-identity.md) covers keys, other people and agents.

## 4. What the import keeps, and what it leaves behind

The import carries the history of every local branch and the tags on it:

- every commit's tree, files, executable bits and symbolic links, its message, and its author's
  and committer's email and date;
- each local branch, as a pallet of the same name in lower case (characters a pallet name does not
  allow become `-`);
- each annotated tag, with its tagger's email, its date and its message.

It does not carry everything. Check this list against your repository before you rely on the
import:

| Git has | After the import |
|---------|------------------|
| A person's name | Gone. Forklift records the email as the operator and keeps no names. |
| A commit's GPG or SSH signature | Gone, and the import does not say so. The history is unsigned until you enroll. |
| A tag's signature | Gone. The import says how many tags carried one. |
| A commit message that is not UTF-8 (`i18n.commitEncoding`) | Changed: each byte that is not UTF-8 becomes `�`, and the import does not say so. |
| A lightweight tag | A tag recorded by your operator, dated at the import. |
| Branches that are not local (`origin/…`) | Not imported. Make a local branch for each first (section 2). |
| Any other ref: notes, the stash, `refs/remotes`, replace refs | Not imported, and the import does not say so. |
| A tag that Forklift cannot name, on a tree or blob, or on a commit no local branch holds | Left out, named with the reason. |
| A submodule | Skipped, with a warning. |
| The commit ids | New ones. Every parcel has its own id, and a later `git export` writes new commit ids again. |

A branch name that would become an invalid pallet name, or two branches that would become the same
one (`Feature` and `feature`), stop the import before it starts, naming them. Rename them in git
and import again.

## 5. Habits that carry over, and the ones that do not

Git's names for the everyday commands work as hidden aliases: `forklift commit` runs `stack`,
`forklift status` runs `stocktake`, `forklift push` runs `lift`. The [guide
index](README.md#the-warehouse-model-in-one-minute) maps each command, and the
[glossary](glossary.md) maps the terms. Most of what you do daily works the way you expect:

```console
$ forklift pallet create docs
Created pallet "docs" and shifted to it.
$ echo 'How to use it.' > usage.md
$ forklift load usage.md
$ forklift stack -m "Document usage"
Stacked parcel … on pallet "docs".
$ forklift shift main
Shifted to pallet "main" (head …).
$ forklift consolidate docs
Fast-forwarded "main" to the head of "docs" (…).
```

These are the differences that catch people coming from git:

- **`pallet create` switches to the new pallet**, as `git switch -c` does. `forklift branch <name>`
  is refused, naming `pallet create <name>` instead.
- **`stack` never stages for you.** There is no `commit -a`: run `forklift load .` first, which
  stages new, changed and deleted files alike. `stack` needs `-m`: it opens no editor.
- **Pallets are not renamed**, and a deleted pallet comes back with `undo`.
- **`lower` is `pull`**: it fast-forwards, or merges the remote's head when the two have diverged.
- **`park` is `stash`**, with named parks every bay shares (`park list`, `park pop <name>`).
- **A bay is a worktree**: `forklift bay add <name>` checks out a new pallet in a working
  directory of its own, sharing the warehouse's history.
- **A haul is a pull request** that lives in the warehouse, so it lifts and lowers with the
  history instead of sitting on a hosting site.
- **`undo` reverses the last operation**, including a stack, a shift or a pallet delete.

Once you work in Forklift, stop committing in git in the same folder. Forklift does not read git's
new commits: to it, a file a later git commit changed is just a local change. Keep `.git` as a
backup until you are sure, then delete it.

## 6. Moving a team

When the warehouse works for you, share it through a server. [Setting up a team](team.md) starts
from a warehouse with an office, which this one now is: it runs a server with a token per person,
lifts the project, and admits a second person who clones it.

Two parts of a move are not there yet, so plan around them:

- **Running beside a live git repository.** The import runs once. Run it again and it refuses,
  because the branches it would create are already pallets. A team cannot keep importing a git
  repository that people still commit to. What works today is the other direction: make Forklift
  the place people commit, and let [`forklift git mirror`](cli.md#git-mirror--keep-a-git-remote-in-step)
  keep a git remote in step for anyone who has not switched yet.
- **CI.** Forklift has no CI integration of its own yet. A CI system that builds from git can build
  from the mirror's branches.

## 7. If you want to go back

[Leaving Forklift](leaving.md) exports a warehouse into a new git repository, including the pallets
that are only on the server, and says which records git has nowhere to keep. The same history
always exports to the same commits, so trying Forklift costs you nothing you cannot take back.

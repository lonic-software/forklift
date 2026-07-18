# Testing doctrine: a contract must be able to fail a test

## Why this exists

A batch of stacked changes to the durability and parallelism primitives (the
`WriteBatch` finish contract, the `TaskExecutor` join guarantee, the transfer-pool
error paths) went through several adversarial review rounds. Across those rounds the
same defect shape recurred more than twenty times:

> A doc comment or contract asserts invariant **X**. The code does **not-X** on some
> reachable path. The next fix is written against the false comment, and introduces a
> new not-X somewhere adjacent.

The reason it kept happening is not carelessness. It is that **the contracts were
load-bearing for correctness and unfalsifiable** — natural-language prose about what
happens when something fails, with no test that would go red if the prose were wrong.
Prose that nothing checks drifts, and once it has drifted every fix built on it inherits
the error. Two concrete instances from the batch: a "best-effort, attempts every
directory" comment sitting above a loop that `?`-returned on the first failure; and an
"every caller joins its workers before finishing" comment above a caller that called
`abort_all()` without waiting.

The rounds stopped finding live defects the moment the contracts became testable — a
mutation-verified directory-sync counter, a pinnable worker count, fault-injection seams
— not because the code became more careful, but because "is this claim true?" turned from
a thing you argue into a thing you run.

## The category is error-path correctness under concurrency — not "crash safety"

It is tempting to call this "crash-safety work," because the primitives describe
themselves in durability vocabulary (`barrier`, `durable`, `crash interleaving`). That
label is wrong and it under-scopes the problem. Of the defects in the batch, exactly one
required a crash (a rename made visible but not directory-fsynced, exposed only by a power
loss). Every other one fired on an **ordinary failure on a concurrent path**:

| Defect | Trigger | Crash needed? |
|--------|---------|---------------|
| Leak check misreads an in-flight producer as failed | one unreadable file, normal run | no |
| Reported error inverted (last failure wins) | two tasks fail together | no |
| Shard names a blob that never landed | disk fills mid-walk (ENOSPC) | no |
| Zombie transfer task writes after lock release | a failed GET + a second process | no |
| Flaky durability test | a small CI runner | no |
| Parcel references a dropped object | error → retry → **power loss** | **yes** |

The real category is: **what happens when an ordinary failure (an I/O error, a task
failure, a full disk, a dropped connection) occurs while multiple things are running in
parallel.** Crash-across-power-loss is one slice of that, not the frame. These paths are
under-tested for two structural reasons — they are rarely exercised (you do not hit ENOSPC
in normal development), and concurrency means a failure *races its siblings* instead of
staying isolated.

Anything that audits or reviews this class must scope to **the failure and cleanup paths
of concurrent code** — error propagation, cancellation, partial failure, what happens to
sibling tasks when one dies — of which durability across a crash is one bullet.

## The principle: a test is the source of truth for a contract

"Treat the code as the source of truth, docs must match it" is half right, and the wrong
half is dangerous. In this batch the *code* was sometimes the bug. Blindly syncing docs to
code would have carefully documented the defects.

The correct hierarchy:

- **A test is the source of truth for a *contract*.** It encodes intent *and* is checked
  against the code on every run. If a guarantee matters, it is a test.
- **Code is the source of truth for *behavior*** — what actually happens.
- **A doc/comment is a human-readable projection of the contract.** When it is
  load-bearing (its being wrong would cause a bug), it must be *backed by* a test that
  fails if the claim is false. When it is mere narration, it is not policed.

So the rule is never "code wins" or "docs win." It is: **if a claim matters, make it a
test — then code and docs are both checked against it, every run.**

## Practices

In priority order.

### 1. Mutation testing in CI (the load-bearing one)

Run [`cargo-mutants`](https://mutants.rs) over `forklift-core`'s failure/concurrency
modules and gate merge on it for changed files. A *surviving mutant* — a line the tool
could break with no test going red — is a test that cannot fail its own contract, which is
worse than no test because it sells false confidence. This automates, deterministically,
the manual check that caught every vacuous test in the batch ("delete the drain — does a
test go red?").

Use the tool, not an LLM agent, for this. "Can this test fail?" is a fact you run, not a
judgment you reason about — reasoning about it is exactly how the batch mis-measured a
flake threshold twice.

### 2. One-time contract audit of the failure-path primitives

Enumerate the load-bearing contract comments on the concurrent-failure primitives —
`WriteBatch`, `TaskExecutor`, the object store's visibility/retry model, the transfer
pool, the pack machinery. For each, ask one question: **is there a test that fails if this
claim is false?** If no → add one, or delete the claim. This is finite (roughly a dozen
primitives) and it is "fix the class" applied at the repo level. Scope it to *failure and
cleanup paths*, per the category note above — not just durability claims.

### 3. Changed contract ⇒ changed test (enforceable, not a checklist)

"Every PR updates its docs" is unenforceable and gets skipped. The enforceable form:
**if a PR changes a documented contract, it must change the test that pins that
contract.** No such test exists? That is the finding — the contract was never testable.
This makes doc-currency a *side effect* of test-currency, which CI and review can actually
check.

### 4. A diff-bounded contract/doc coherence check in review

Add one dimension to the existing code-review workflow: *does this diff change a contract
comment without changing its pinning test, or leave a comment near the changed code
asserting something the change made false?* This is a good scoped LLM task because it is
bounded to the diff.

**Do not** build a standing, repo-wide "documentation agent" that continuously syncs
comments. It chases the symptom (drift) instead of the disease (untestable contracts), it
produces unbounded noise, and — the fatal flaw — it will confidently sync a doc to match
buggy code. The bounded, in-review version is good; the standing sweeper manufactures the
problem it claims to solve.

## What is *not* in scope here

None of the above resolves the object store's **visible ⟹ durable** assumption — a retry
treats any visible object name as durably stored, but a barrier error can leave a name
visible with a non-durable directory entry. That is a genuine *design decision* (never
return an error with visible-but-unsynced names / do not trust visibility after a failed
barrier / accept the window and stop claiming crash-equivalence), not a test-coverage gap.
It needs a decision, not a mutation test. Keep it on a separate track so the process work
above does not get tangled with it.

## The one-line version

If a guarantee about what happens when something fails matters enough to write down, it
matters enough to make a test that goes red when it is false. Everything else here is
machinery for enforcing that one sentence.

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
the error. Two concrete instances from that batch: a "best-effort, attempts every
directory" comment sitting above a loop that `?`-returned on the first failure; and an
"every caller joins its workers before finishing" comment above a caller that called
`abort_all()` without waiting. Both are fixed today — `TaskExecutor::execute`
(`crates/forklift-core/src/model/task.rs`) now documents, and enforces with a drain
loop, that it never returns while a worker's task body is still running (see its own
doc comment for the exact `abort_all`-only-signals-cancellation reasoning); the
transfer pool's `join_all`/`drain_remaining` pair (`crates/forklift-core/src/util/remote_utils.rs`)
carries the equivalent guarantee for network tasks.

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

### 1. Mutation testing in CI — a lane that catches deletions, not substitutions

Run [`cargo-mutants`](https://mutants.rs) over `forklift-core`'s failure/concurrency
modules and gate merge on it for changed files. A *surviving mutant* — a line the tool
could break with no test going red — is a test that cannot fail its own contract, which is
worse than no test because it sells false confidence. As of this writing there is no such
job in `.github/workflows/`; this is still a proposal, not a shipped gate.

**What it covers.** `cargo-mutants`' documented mutation genres (`mutants.rs`'s
mutation-patterns page, mirrored in the project's own book source at
`book/src/mutants.md`) are: replacing a function's body with a value guessed from its
return type (`FnValue`); substituting one binary operator for another (`==`→`!=`,
`+`→`-`, and similarly for the rest); deleting a unary operator (`-a`→`a`); deleting a
match arm when a wildcard is present; replacing a match guard with `true`/`false`; and
deleting an individual field from a struct literal that has a `..base` expression. That
is the complete documented list. A test suite with a surviving mutant in one of these
shapes — a helper nobody's test would notice returning `0` instead of the real value, a
comparison nobody's test would notice flipped — is exactly the "delete the drain, does a
test go red?" check the original batch ran by hand, mechanized.

**What it does not cover.** None of those genres swaps one already-valid, in-scope
expression for another — the tool never rewrites `self.connect_timeout` to read a
sibling field, a different constant, or a hardcoded literal of the same type, because
doing so is not one of its genres. That gap is not theoretical. A recent fix to this
codebase's remote-fetch error path (`crates/forklift-core/src/util/remote_utils.rs`,
under review as PR #93) needed a bounded HTTP client's error-body read to budget against
*that client's own* `connect_timeout` field — a Tor-routed client and a direct one carry
genuinely different values (`REMOTE_CONNECT_TIMEOUT_TOR` = 60s vs. `REMOTE_CONNECT_TIMEOUT`
= 5s, both real constants in that file) — rather than a fixed constant. Pinning that
fix took three attempts:

- The first pinning test asserted a free helper function's own arithmetic, in isolation.
  Nothing required the code under fix to actually call that helper — a caller could
  inline different arithmetic, or skip it, and the test would still pass.
- Reverting the call site back to a bare hardcoded constant (undoing the fix entirely)
  left the full suite green, that test included: it exercised the helper, not the call
  site the fix actually changed.
- The next attempt fixed that by asserting on the call site directly — but the fixture
  it chose replaced `self.connect_timeout` with the sibling constant
  `REMOTE_CONNECT_TIMEOUT_TOR`, and the suite stayed green *again*. The reason: a Tor
  client's `connect_timeout` field genuinely **equals** `REMOTE_CONNECT_TIMEOUT_TOR`
  (60s), and a wholly unrelated constant in the same file, `FETCH_OBJECT_READ_TIMEOUT`,
  also happens to be 60s. A fixture asserting the total budget equals `60s + 10s`
  (`REMOTE_CONNECT_TIMEOUT_TOR` plus the read-silence budget `REMOTE_READ_TIMEOUT`)
  cannot distinguish "this code read the instance field" from "this code hardcoded
  either rival constant" — all three produce the identical number.

No mutation-testing genre in the list above would have generated any of these three
mutants: none of them delete an operator, replace a match arm, or drop a struct-literal
field — they swap one already-valid expression for a same-typed rival, which is
precisely the class `cargo-mutants` does not implement. A mutation-testing gate over
this exact file, running today, would not have caught this.

**The rule this doctrine states directly, because a green revert run is not enough to
trust a pinning test:** a pinning test's fixture must give the source a value that
separates it from every rival the code could have read instead — every constant in
scope, every default, every same-typed field. If the injected value coincides with any
of them, the test cannot tell right from wrong there, whatever the revert run showed.
Where no production constructor can emit a separating value, inject one — a test-only
constructor or a field override built for exactly this exists to make that possible.

**The habit that enforces it:** before trusting a pinning test, enumerate its rivals —
every constant, default, and same-typed field the source under test could have read
instead of the one the fixture actually injects — pick or construct an injected value
distinct from all of them, and record, for the record: the value injected, which rivals
it is separated from, and any collisions found along the way. A collision found during
that enumeration is itself the finding; fix the fixture, not the code.

Use the tool, not an LLM agent, for the part it actually does automate. "Can this test
fail?" against one of the six genres above is a fact you run, not a judgment you reason
about — reasoning about it is exactly how the original batch mis-measured a flake
threshold twice. But the genre list is also the tool's ceiling: it is one lane in a
testing doctrine, not the load-bearing one, and it would not by itself have caught the
class of defect the batch's manual review found.

### 2. One-time contract audit of the failure-path primitives

Enumerate the load-bearing contract comments on the concurrent-failure primitives —
`WriteBatch`, `TaskExecutor`, the transfer pool, the pack machinery, and the durability
taint-and-heal record-and-repair contract (`taint_utils`/`heal_utils`/`recovery_utils`,
which now carries the object store's visibility/retry guarantees — see the note below on
what used to be out of scope here). For each, ask one question: **is there a test that
fails if this claim is false?** If no → add one, or delete the claim. This is finite and
it is "fix the class" applied at the repo level. Scope it to *failure and cleanup paths*,
per the category note above — not just durability claims.

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

## What used to be out of scope here

> **Update — resolved.** When this section was first written, it named the object
> store's **visible ⟹ durable** assumption as a genuine open design decision, deliberately
> kept off this doctrine's process track. That decision has since been made and shipped:
> a failing write now records exactly its own final paths as a durability taint
> (`taint_utils`), an automatic entry-heal chokepoint restages them at the next command's
> entry (`heal_utils::heal_if_tainted`), and a dedicated `forklift heal` verb walks every
> durable ref source for whatever entry-heal alone cannot resolve
> (`recovery_utils`) — full contract in `docs/DESIGN.html` §3.1.1. It is no longer an
> open question this doctrine defers; it is a shipped contract that belongs, like the
> other primitives above, under practice 2's audit.

The general point that section was making still holds for whatever the next such
question turns out to be: some invariants are correctness **decisions** the design has
to make — what a retry may assume about visibility, what an error is allowed to claim
about durability — not coverage gaps a test can paper over. A decision like that needs a
decision, argued and written down, not a mutation test. When one comes up, give it its
own track rather than folding it into this doctrine's process items, so the two kinds of
work — "is this claim tested?" and "is this claim even the right one?" — never get
tangled together.

## The one-line version

If a guarantee about what happens when something fails matters enough to write down, it
matters enough to make a test that goes red when it is false. Everything else here is
machinery for enforcing that one sentence — and knowing which lane actually catches which
class of violation is part of enforcing it honestly.

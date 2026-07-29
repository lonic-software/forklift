# Working on Forklift

**Forklift is pre-1.0 with no users. Do not design around backward compatibility — there is nothing
behind it, and pretending otherwise has already produced worse work than the breaking change would
have.** Reinvent what needs reinventing.

> **Temporary file — delete it when v1.0 ships.** Everything below follows from that status and stops
> being true the moment Forklift has an installed base.

## There is no installed base to protect

The only warehouses that exist are the maintainer's own test and dogfood repositories, and they are
expendable. Nobody is running a version we owe compatibility to.

Treat that as a standing licence to **get things right**, not merely as permission to break things:

- **Anything can change.** On-disk formats, record shapes, the command surface, the wire protocol,
  public signatures — none of it is frozen, and none of it needs a migration path, a compatibility
  shim, or a grandfather clause.
- **Replace rather than patch around.** If something is wrong, awkward, or just worse than an obvious
  alternative, fix the thing itself. Bolting a workaround onto a bad design to avoid disturbing it is
  the wrong trade: the disturbance is free today and will not be later.
- **Scale is not an objection.** "That would be a big change" is not a reason to prefer a small wrong
  one. Propose the substantial version — a large refactor costs less now than it ever will again.
- **Compatibility is not an argument.** Reasoning that begins "but existing warehouses or clients
  would break" is inadmissible until v1. If you find yourself designing compatibility machinery,
  stop and ask who it is for; the answer is usually nobody.

## What the anxiety actually costs — a worked example

This is not hypothetical. A permissions design was drafted in one session under an unexamined
assumption that a shipped record shape had to be preserved. What that single assumption produced:

- a **permanent two-surface split** in the data model, accepted in writing as a cost worth paying;
- an entire hazard section about older binaries silently discarding records, plus a heuristic to tell
  those accidents apart from deliberate edits, plus a split enforcement posture to handle them;
- a **load-bearing claim that was entirely about how already-shipped binaries behave** — so when the
  assumption was dropped, the design was left with no keystone at all;
- a filed ticket to *harden* a field that fails open, when the field could simply be deleted.

None of it was needed. The revision was shorter, and it could express something the compatible
version structurally could not — the very property the feature existed for.

**The tell is worth learning, because it does not look like caution.** Nobody writes "I am adding a
compatibility shim." It shows up as an *accepted cost* — a slightly worse shape, a second code path,
an extra invariant to police — introduced with a reasonable-sounding tradeoff sentence. When you
notice yourself justifying a design as the price of not disturbing something, stop and ask who is
actually behind that thing. Right now, the answer is nobody.

## What this does *not* relax

The freedom is from **backward** compatibility, and from nothing else. The rest matters more while
the design is still moving, not less:

- **Forward-facing contracts still bind.** Rules that constrain clients and warehouses which do not
  exist yet — read semantics, dispatch and tolerance rules, what a verdict is allowed to claim — are
  untouched by "no users". Zero users retires arguments about protecting an installed base; it
  retires nothing about future readers.
- **The evidence bar is unchanged.** A change being free to make is not a change being free to make
  badly. Grounded claims, falsifying tests in both directions, and per-class sweeps are still the
  standard.
- **Silent breakage is still a bug.** Formats may change without migrations; they may not change in
  ways that make old data quietly *misread*. Fail loudly on anything no longer understood — a
  version that cannot be parsed should say so, never guess, and never fall back to a permissive
  default.

## When this expires

Delete this file at **v1.0**, or earlier if Forklift gains a real user or an external deployment —
whichever comes first. From that point the ordinary compatibility rules apply, and any design written
under this licence should be re-read knowing it assumed a freedom that no longer exists.

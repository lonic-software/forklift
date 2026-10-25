<!--
GENERATED FILE — do not edit by hand.
Produced by `bin/gen-docs` (crates/forklift/src/docgen.rs). CI and `bin/check` run
`bin/gen-docs --check`, which fails when this file is stale; run `bin/gen-docs` and commit
the result after a change that affects it (an error code, a command, alias or help line,
or a `--json` output struct).
-->

# Error codes and exit codes

Every `forklift` failure carries a stable `code` (in the `--json` error envelope) and the process exits with the matching deterministic status, so a script or an agent branches without parsing prose. `0` (success), `1`, `2` (`usage`), `130` (cancelled by the user) and `128`+n (killed by signal n) mean the same for every Lonic tool; every other number is Forklift's own. Both tables are generated from the single `ErrorCode` enum in `crates/forklift/src/output.rs` — see `docs/guide/cli.md` for how a script is meant to use them.

Exit code 17 is reserved for a future feature and is not yet assigned to any code.

| `code` | exit | Meaning |
|---|---|---|
| `error` | 1 | Anything without a more specific classification yet |
| `usage` | 2 | The command line could not be parsed (an unknown command, a missing or invalid argument) |
| `not_a_warehouse` | 3 | The command needs a warehouse; this directory is none |
| `conflict` | 4 | Working state blocks the operation (unresolved / dirty) |
| `diverged` | 5 | The local pallet and the remote's both hold parcels the other lacks; nothing was lifted or lowered |
| `warehouse_locked` | 6 | Another forklift process holds the warehouse lock |
| `out_of_scope` | 7 | A path argument is outside a scoped (sparse) bay's scope |
| `scope_path_type_changed` | 8 | A scoped bay's spine path flipped dir↔file; scope no longer valid |
| `sparse_workspace` | 9 | A whole-tree verb is not supported in a scoped (sparse) bay yet |
| `out_of_scope_conflict` | 10 | A scoped bay merge hit an out-of-scope entry changed on both sides |
| `non_origin_lift` | 11 | A sparse workspace tried to lift to a remote other than its origin |
| `narrow_unclean` | 12 | "narrow" would delete a subtree that still holds uncommitted work |
| `scope_prune_blocked` | 13 | "scope-prune" would free a path a checkout still materializes |
| `chunked_transport_unsupported` | 14 | A chunked large file can't go into a bundle, or is being lifted to a remote that doesn't support chunking |
| `oversized_transport_unsupported` | 15 | An object predates the size limit and can't be sent to a remote or bundle |
| `commit_pagination_unsupported` | 16 | A lift needs a paginated commit (many objects) and the remote doesn't support it yet |
| `query_predicate_invalid` | 18 | A "query" predicate is malformed or exceeds a fixed bound |
| `empty_history` | 19 | "history" was asked to walk a pallet that has nothing stacked on it yet |
| `incomplete_load` | 20 | "stack", "park", "consolidate" or "cherry-pick" refused because a "load" that never finished cleanly is still recorded, or a command refused because an operation that rewrites the whole staging area was interrupted ("forklift heal" finishes or undoes it) |
| `durability_taint` | 21 | A durability taint could not be auto-healed (a vanished, unreadable, or corrupt path, or a torn taint record) |
| `trust_changed` | 22 | A remote's trust anchor is not the one this warehouse holds or the command named; the details carry its current value |
| `remote_protocol_violation` | 23 | A remote answered with something the protocol never produces; it is misbehaving or corrupt, and retrying gets the same answer |
| `meta_conflict` | 24 | A meta pallet changed differently on both sides at the same path; nothing was lifted for it, and "lower --keep" resolves each named path |
| `staged_content_missing` | 25 | "stack" or "park" found a staged file whose content the object store no longer holds; no parcel was made |
| `behind` | 26 | Another bay or process moved the pallet past the parcel this bay stands on; nothing was moved, and "shift" catches the bay up |
| `target_not_covered` | 27 | "deliver" was aimed at a target holding changes the draft has not seen; consolidating the target into the draft comes first |
| `remote_ahead` | 28 | The remote's pallet holds parcels this warehouse has not lowered yet; "lower" comes first |
| `pallet_not_found` | 29 | No pallet of that name exists in this warehouse (or, for "lower", on the remote); "lower <name>" brings in the remote's, "palletize <name>" creates a new one, "palletize --remote" lists the remote's |
| `franchise_incomplete` | 30 | The directory holds a franchise that has not finished; running the same "franchise" again resumes it and keeps what was fetched |
| `only_prefix_not_found` | 31 | A sparse franchise's --only path is not a directory in the pallet on the remote |
| `staged_upload_discarded` | 32 | A lift's commit named an upload the remote no longer holds (discarded or never received); running the lift again uploads it afresh |
| `mirror_incomplete` | 33 | A mirror run left at least one pallet out of step with git; the others went through, and each failed pallet's message names its step |


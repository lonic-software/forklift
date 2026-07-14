<!--
GENERATED FILE — do not edit by hand.
Produced by `bin/gen-docs` (crates/forklift/src/docgen.rs). `bin/check` regenerates and
diffs this file to catch drift; run `bin/gen-docs` and commit the result after a change
that affects it (a new error code, or a `--json` output struct).
-->

# Error codes and exit codes

Every `forklift` failure carries a stable `code` (in the `--json` error envelope) and the process exits with the matching deterministic status, so a script or an agent branches without parsing prose. `2` is reserved for clap's own argument/usage errors; `0` is success. Both tables are generated from the single `ErrorCode` enum in `crates/forklift/src/output.rs` — see `docs/guide/cli.md` for how a script is meant to use them.

Exit codes 17 and 18 are reserved for future features and are not yet assigned to any code.

| `code` | exit | Meaning |
|---|---|---|
| `error` | 1 | Anything without a more specific classification yet |
| `not_a_warehouse` | 3 | The command needs a warehouse; this directory is none |
| `conflict` | 4 | Working state blocks the operation (unresolved / dirty) |
| `diverged` | 5 | A remote ref moved under a lift — lower, retry |
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


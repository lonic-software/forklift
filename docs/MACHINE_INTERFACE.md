# The machine-first interface

Forklift's command surface is built to be driven by programs — scripts, CI, and AI
coding agents — as well as people (DESIGN.html §7.4). Three things make that work:
a `--json` mode with a versioned envelope, a stable error and exit-code taxonomy, and
an MCP server that exposes every command as a schema-typed tool.

## `--json`

`--json` is a global flag: add it to any command and stdout becomes exactly **one**
JSON document (nothing else prints there). Human prose is unchanged without the flag.

Success envelope:

```json
{
  "forklift_json": "1",
  "command": "stocktake",
  "ok": true,
  "data": { "…command-specific…" }
}
```

Failure envelope (also sets the exit code below):

```json
{
  "forklift_json": "1",
  "ok": false,
  "error": {
    "code": "not_a_warehouse",
    "message": "…",
    "next_step": "Run \"forklift prepare\" to create a warehouse here, or change into one."
  }
}
```

`forklift_json` is the output schema version. It changes only when the envelope or a
command's `data` shape changes incompatibly, so a consumer can pin it. A command's
`data` shape is documented by the struct it emits (in `crates/forklift/src/commands/`).

Token-cheap by default: `stocktake --summary` reports counts only (no per-path lists),
and `diff --json` reports the changed-file set (path + kind) rather than every line —
a program reads specific content by hash when it needs it.

## Error codes and exit codes

Every failure carries a stable `code` an agent can branch on, and the process exits
with a deterministic status (§7.8) so a script can branch without parsing prose. `2`
is reserved for argument/usage errors (clap); `0` is success.

| `code`                    | exit | Meaning                                                          |
|---------------------------|------|-------------------------------------------------------------------|
| `error`                   | 1    | Anything without a more specific classification yet               |
| `not_a_warehouse`         | 3    | The command needs a warehouse; this directory is none             |
| `conflict`                | 4    | Working state blocks the operation (unresolved / dirty)           |
| `diverged`                | 5    | A remote ref moved under a lift — lower, retry                     |
| `warehouse_locked`        | 6    | Another forklift process holds the warehouse lock                 |
| `out_of_scope`            | 7    | A path argument is outside a scoped (sparse) bay's scope (§7.6)   |
| `scope_path_type_changed` | 8    | A scoped bay's spine path flipped dir↔file; scope no longer valid |
| `sparse_workspace`        | 9    | A whole-tree verb is not supported in a scoped (sparse) bay yet   |

The codes and exit numbers are a contract: they get added to, never repurposed.

## Structured conflicts

`forklift conflicts` lists the files an unresolved consolidation or cherry-pick left in conflict.
With `--json`, each file's three sides are **content addresses** — blob hashes a
resolver fetches (`forklift peek <hash>`) and diffs, instead of parsing marker soup:

```json
{
  "data": {
    "conflicts": [
      { "path": "f.txt", "markers": true,
        "base": "<hash>", "ours": "<hash>", "theirs": "<hash>" }
    ]
  }
}
```

A whole-file or binary conflict has `markers: false` and no sides. An empty list is a
valid answer — nothing to resolve — not an error.

## `forklift mcp` — the MCP server

`forklift mcp` runs a Model Context Protocol server on stdin/stdout (newline-delimited
JSON-RPC 2.0). Point an MCP client — an AI coding tool — at it, with the warehouse as
the working directory. It implements `initialize`, `tools/list` and `tools/call`.

Each tool re-invokes `forklift … --json` and returns its envelope, so the tools speak
the exact structured output above (and inherit the warehouse lock and exit-code
taxonomy). A command that exits non-zero comes back as an MCP tool error (`isError:
true`) carrying the error envelope — the agent sees the stable `code`/`next_step`, not
a crashed session.

The tool surface **mirrors the CLI** — every CLI command is exposed as a tool (a
multi-subcommand command becomes `<command>_<subcommand>` tools) or is on a small
human-only allow-list; a unit test (`every_cli_command_is_an_mcp_tool_or_explicitly_human_only`)
fails CI if that ever drifts. Tools (arguments in parentheses):

- **Inspect:** `stocktake` (summary?), `history` (revision?, class?, limit?, after?),
  `diff` (staged?, targets?), `peek` (object | inventory), `blame` (path, rev?),
  `audit` (pallet?), `conflicts`.
- **Change:** `load` (path), `unload` (path), `stack` (description?), `restore` (path,
  staged?), `undo`, `park` / `park_list` / `park_pop`, `cherry_pick` (revision, message?),
  `deliver` (target, message?).
- **Maintain:** `compact` (all?) — pack the loose object store into a few dense pack files
  (safe to run anytime; worth running after a large import). `all=true` is a full repack:
  also rewrite existing packs, dropping unreachable garbage and consolidating.
- **Branch / merge:** `shift` (pallet), `consolidate` (pallet), `palletize` (name?,
  revision?, all?).
- **Remote:** `lift`, `lower`.
- **Review & metadata:** `manifest_note` / `manifest_approve` / `manifest_provenance`
  (model, transcript?, message?) / `manifest_show`, `haul_open` / `haul_list` / `haul_show` /
  `haul_comment` / `haul_review` / `haul_merge` / `haul_close` / `haul_reopen`,
  `tag_create` / `tag_show` / `tag_list`, `office_list`.
- **Sandboxing (§7.5, §7.6):** `bay_add` (name, path?, scope?), `bay_list`, `bay_remove`
  (name), `scope` — an orchestrator agent opens task-scoped (optionally sparse) sandboxes for
  its sub-agents directly. The scope a bay records is advisory local setup, not the agent's
  own security boundary; enforcement of what an identity may touch lives remote-side
  (FORK-10).

**Pagination:** `history` reads in pages — pass `limit`, and the result's `data.next`
cursor back as `after` for the following page (absent once exhausted). This is the
agent-facing counterpart of the CLI's pager (agents get a cursor, never a pager).

**Provenance is transport-derived, not self-reported (§7.2).** For `manifest_provenance`
the server sets the `tool` from the connection's `clientInfo` (the harness that drove the
model) and mints the `session` itself — overriding anything the agent passes, so a model
cannot fabricate its own `tool`/`session` in the tool-call arguments. That is why those two
fields are **not** in the tool's schema. `model` stays the agent's attestation: MCP carries
no model identity, so nothing at the transport can supply or verify it. As always the entry
is *signed*, so who recorded it is forge-proof; the transport-derivation just removes the
model's own output from the `tool`/`session` it can't be trusted to report about itself.

**Not exposed** (deliberately human-only): warehouse/identity setup (`prepare`, `config`,
`profile`, `franchise`, `import-git`, `export-git`), the host-machine concerns `alias` and
`self-update`, and meta (`mcp`, `help`, `version`). `config` in particular can rewrite
`remote.url` / `remote.token`, which is not an agent-workflow action; the `office`
*mutations* (enrol/admit/rotate/…) are likewise held back — an agent operates within a
warehouse whose trust is already set up. (`office_list` is exposed, read-only.)

`bay` **is** exposed, despite also being a host working directory: §7.6's agent story is an
orchestrator creating task-scoped sandboxes for sub-agents over MCP, and `bay` is how it does
that. Every bay operation is non-destructive — `bay_add` refuses onto a non-empty directory,
`bay_remove` only deletes forklift's own bookkeeping, never the materialized files.

Example session:

```
→ {"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}
← {"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"forklift","version":"…"}}}
→ {"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"stocktake","arguments":{"summary":true}}}
← {"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"{…forklift envelope…}"}],"isError":false}}
```

## Notes for implementers

* Nothing but the envelope (or the MCP protocol messages) reaches stdout under
  `--json` / `mcp`. Progress chatter is suppressed; the result is a single document.
* Human output is untouched by all of this — the same commands print prose without
  `--json`, byte-for-byte as before.

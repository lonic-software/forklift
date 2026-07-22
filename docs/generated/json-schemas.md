<!--
GENERATED FILE — do not edit by hand.
Produced by `bin/gen-docs` (crates/forklift/src/docgen.rs). `bin/check` regenerates and
diffs this file to catch drift; run `bin/gen-docs` and commit the result after a change
that affects it (a new error code, or a `--json` output struct).
-->

# `--json` output schemas

Every command's `--json` result is `{ "forklift_json", "command", "ok": true, "data": … }` on success (see `docs/MACHINE_INTERFACE.md` for the envelope and the failure shape). This page is the exhaustive reference for each command's `data` — one [JSON Schema](https://json-schema.org/) per shape a command can emit; a command with more than one (e.g. one per subcommand) lists all of them. Descriptions come straight from the Rust doc comments on the underlying struct, so they stay in sync with the field they describe.

A command not listed here either reports only the generic human-message shape `{ "message": string }`, or produces no `--json` data at all — see the command's entry in `docs/guide/cli.md`.

## `alias`

### `Installed`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The result of creating (or confirming) the alias.",
  "properties": {
    "already_installed": {
      "description": "Whether the alias already existed and pointed here (idempotent no-op) vs. was just\ncreated by this run.",
      "type": "boolean"
    },
    "name": {
      "type": "string"
    },
    "path": {
      "type": "string"
    },
    "target": {
      "type": "string"
    }
  },
  "required": [
    "name",
    "path",
    "target",
    "already_installed"
  ],
  "title": "Installed",
  "type": "object"
}
```

### `Uninstalled`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The result of removing (or confirming the absence of) the alias.",
  "properties": {
    "name": {
      "type": "string"
    },
    "path": {
      "type": "string"
    },
    "removed": {
      "description": "Whether anything was actually removed (`false` if it was already absent).",
      "type": "boolean"
    }
  },
  "required": [
    "name",
    "path",
    "removed"
  ],
  "title": "Uninstalled",
  "type": "object"
}
```

### `StatusReport`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "Whether the alias exists, and where it points.",
  "properties": {
    "installed": {
      "type": "boolean"
    },
    "name": {
      "type": "string"
    },
    "path": {
      "type": "string"
    },
    "target": {
      "type": [
        "string",
        "null"
      ]
    }
  },
  "required": [
    "name",
    "path",
    "installed"
  ],
  "title": "StatusReport",
  "type": "object"
}
```

## `audit`

### `AuditReport`

```json
{
  "$defs": {
    "AuditScope": {
      "description": "What a content audit checked, and (on a sparse warehouse) the boundary it sealed rather than\nverified. Emitted so a skimming reader or agent can never mistake a sparse pass — which verified\ncontent only within the fetch scope — for a full-clone pass, nor a presence-only pass for a\n`--full` re-read.",
      "properties": {
        "chunks": {
          "description": "How a chunked file's chunks were checked: `presence-checked` in a normal audit (bounded, no\nbytes re-read), or, under `--full`, re-read and re-hashed with each file re-assembled to\nverify its recipe's content hash.",
          "type": "string"
        },
        "enforcement": {
          "description": "The scope boundary is advisory — a client choice, not enforced by the remote. Present only\non a sparse warehouse.",
          "type": [
            "string",
            "null"
          ]
        },
        "fetch_scope": {
          "description": "The warehouse fetch scope: the path prefixes whose content was fetched. Present only on a\nsparse warehouse; a full clone omits it (nothing is out of scope). Everything outside is\nsealed by hash, not downloaded.",
          "items": {
            "type": "string"
          },
          "type": [
            "array",
            "null"
          ]
        },
        "in_scope_content": {
          "description": "In-scope content: every tree was re-hashed on read and every blob confirmed present.",
          "type": "string"
        },
        "out_of_scope_content": {
          "description": "Out-of-scope content: sealed by the hash a signed parcel commits (unforgeable), verified\nwhen it is fetched. Present only on a sparse warehouse.",
          "type": [
            "string",
            "null"
          ]
        },
        "signatures": {
          "description": "Signatures — the office chain and every parcel — are verified in full regardless of\nscope (parcels and their sidecars are always fully present).",
          "type": "string"
        }
      },
      "required": [
        "signatures",
        "in_scope_content",
        "chunks"
      ],
      "type": "object"
    }
  },
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The result of an offline audit: the office chain always, plus a working pallet's\nparcel counts when one (not the office) was audited.",
  "properties": {
    "full": {
      "description": "Whether this was a `--full` audit: every present chunk's bytes were re-read and re-hashed,\nand each fully-present chunked file re-assembled to verify its recipe's content hash. A\nnormal audit presence-checks chunks without re-reading them.",
      "type": "boolean"
    },
    "genesis": {
      "description": "The genesis the office chain verified back to.",
      "type": "string"
    },
    "legacy_parcels": {
      "description": "How many legacy (pre-trust, unsigned) parcels were tolerated.",
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    },
    "pallet": {
      "description": "The pallet that was audited.",
      "type": "string"
    },
    "pallet_verified": {
      "description": "Whether a working pallet's history was audited (absent when only the office\nchain was — i.e. the audited pallet *is* the office).",
      "type": [
        "boolean",
        "null"
      ]
    },
    "scope": {
      "anyOf": [
        {
          "$ref": "#/$defs/AuditScope"
        },
        {
          "type": "null"
        }
      ],
      "description": "The content audit that ran: present when the warehouse is sparse (to report the sealed\nboundary) or when `--full` re-verified content on a full clone. A normal full-clone audit is\nsignature-only and omits it, so a partial or presence-only pass is never mistaken for a\ncomplete content re-verification."
    },
    "verified_parcels": {
      "description": "How many signed parcels verified on the pallet.",
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    }
  },
  "required": [
    "genesis",
    "pallet",
    "verified_parcels",
    "legacy_parcels",
    "full"
  ],
  "title": "AuditReport",
  "type": "object"
}
```

## `bay`

### `BayCreated`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "A newly created bay.",
  "properties": {
    "head": {
      "type": "string"
    },
    "name": {
      "type": "string"
    },
    "pallet": {
      "type": "string"
    },
    "path": {
      "type": "string"
    },
    "scope": {
      "description": "The bay's materialization scope prefixes (empty for a full, unscoped bay).",
      "items": {
        "type": "string"
      },
      "type": "array"
    }
  },
  "required": [
    "name",
    "path",
    "pallet",
    "head",
    "scope"
  ],
  "title": "BayCreated",
  "type": "object"
}
```

### `BayList`

```json
{
  "$defs": {
    "BayEntry": {
      "description": "One bay in the list.",
      "properties": {
        "name": {
          "type": "string"
        },
        "pallet": {
          "description": "The bay's current pallet (`null` when unreadable).",
          "type": [
            "string",
            "null"
          ]
        },
        "path": {
          "type": "string"
        }
      },
      "required": [
        "name",
        "path"
      ],
      "type": "object"
    }
  },
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The list of bays.",
  "properties": {
    "bays": {
      "items": {
        "$ref": "#/$defs/BayEntry"
      },
      "type": "array"
    }
  },
  "required": [
    "bays"
  ],
  "title": "BayList",
  "type": "object"
}
```

### `BayRemoved`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "A removed bay.",
  "properties": {
    "name": {
      "type": "string"
    }
  },
  "required": [
    "name"
  ],
  "title": "BayRemoved",
  "type": "object"
}
```

## `blame`

### `Blame`

```json
{
  "$defs": {
    "BlameLine": {
      "description": "One line of the blamed file.",
      "properties": {
        "content": {
          "description": "The line content (without its trailing newline).",
          "type": "string"
        },
        "number": {
          "description": "The 1-based line number.",
          "format": "uint",
          "minimum": 0,
          "type": "integer"
        },
        "parcel": {
          "description": "The hash of the parcel that introduced the line (a key into `parcels`).",
          "type": "string"
        }
      },
      "required": [
        "number",
        "parcel",
        "content"
      ],
      "type": "object"
    },
    "BlamedParcel": {
      "description": "A parcel a line is attributed to, with its author resolved to signed identity metadata.",
      "properties": {
        "class": {
          "description": "The author's identity class, when it is not a plain human — so agent, bot and\nservice authorship is legible in the blame.",
          "type": [
            "string",
            "null"
          ]
        },
        "name": {
          "description": "The resolved display name, when a resolution hook supplied one.",
          "type": [
            "string",
            "null"
          ]
        },
        "operator": {
          "description": "The primary author's pseudonymous operator id (the chain's record).",
          "type": "string"
        },
        "supervisor": {
          "description": "The supervising human of an automated author, when one is recorded.",
          "type": [
            "string",
            "null"
          ]
        },
        "timestamp": {
          "description": "The author action's time as RFC 3339 (UTC).",
          "type": "string"
        }
      },
      "required": [
        "operator",
        "timestamp"
      ],
      "type": "object"
    }
  },
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The blame of a file: its lines, and the parcels they are attributed to.",
  "properties": {
    "lines": {
      "items": {
        "$ref": "#/$defs/BlameLine"
      },
      "type": "array"
    },
    "parcels": {
      "additionalProperties": {
        "$ref": "#/$defs/BlamedParcel"
      },
      "description": "The distinct blamed parcels, keyed by hash (so a line carries only the hash).",
      "type": "object"
    },
    "path": {
      "type": "string"
    },
    "revision": {
      "description": "The revision the blame was taken at (the resolved head parcel hash).",
      "type": "string"
    }
  },
  "required": [
    "path",
    "revision",
    "parcels",
    "lines"
  ],
  "title": "Blame",
  "type": "object"
}
```

## `cherry-pick`

### `CherryPicked`

```json
{
  "$defs": {
    "CherryPickOutcome": {
      "description": "What a cherry-pick did. `Conflicts` is the only outcome that leaves work for the operator\n(resolve, load, stack); `Applied` is complete.",
      "enum": [
        "applied",
        "conflicts"
      ],
      "type": "string"
    }
  },
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The result of a cherry-pick.",
  "properties": {
    "conflicts": {
      "description": "The conflicting paths, when the pick did not complete cleanly.",
      "items": {
        "type": "string"
      },
      "type": "array"
    },
    "outcome": {
      "$ref": "#/$defs/CherryPickOutcome"
    },
    "pallet": {
      "description": "The pallet the pick applied to (the current one).",
      "type": "string"
    },
    "parcel": {
      "description": "The new parcel, when the pick completed cleanly.",
      "type": [
        "string",
        "null"
      ]
    },
    "source": {
      "description": "The parcel that was picked.",
      "type": "string"
    }
  },
  "required": [
    "outcome",
    "source",
    "pallet",
    "conflicts"
  ],
  "title": "CherryPicked",
  "type": "object"
}
```

## `compact`

### `Compacted`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The result of a compaction.",
  "properties": {
    "all": {
      "description": "Whether this was a full repack (existing packs rewritten, garbage dropped).",
      "type": "boolean"
    },
    "bytes_packed": {
      "description": "Total bytes written into the packs (delta-compressed where deltas were used).",
      "format": "uint64",
      "minimum": 0,
      "type": "integer"
    },
    "corrupt_skipped": {
      "description": "Loose objects skipped because their bytes did not decode or did not hash to their\nfilename address; left in place rather than packed or removed.",
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    },
    "deltas": {
      "description": "Of the packed objects, how many were stored as deltas against a similar base.",
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    },
    "loose_removed": {
      "description": "Original files removed after their pack was durably written.",
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    },
    "objects_packed": {
      "description": "Objects packed.",
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    },
    "over_ceiling_skipped": {
      "description": "Loose objects skipped because decoding them would exceed the 64 MiB object ceiling —\nlegitimate objects authored before that ceiling existed, not corruption; left in place\nrather than packed or removed.",
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    },
    "packs_written": {
      "description": "Packs written (more than one when the set crossed a rollover threshold).",
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    }
  },
  "required": [
    "all",
    "objects_packed",
    "packs_written",
    "loose_removed",
    "deltas",
    "bytes_packed",
    "corrupt_skipped",
    "over_ceiling_skipped"
  ],
  "title": "Compacted",
  "type": "object"
}
```

## `config`

### `ConfigSet`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "A `config <key> <value>` set (human output stays silent).",
  "properties": {
    "key": {
      "type": "string"
    },
    "value": {
      "type": "string"
    }
  },
  "required": [
    "key",
    "value"
  ],
  "title": "ConfigSet",
  "type": "object"
}
```

### `ConfigValue`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "A `config <key>` read.",
  "properties": {
    "key": {
      "type": "string"
    },
    "value": {
      "type": "string"
    }
  },
  "required": [
    "key",
    "value"
  ],
  "title": "ConfigValue",
  "type": "object"
}
```

### `ConfigList`

```json
{
  "$defs": {
    "ConfigEntry": {
      "description": "One known configuration key and its effective value (if set).",
      "properties": {
        "key": {
          "type": "string"
        },
        "scope": {
          "description": "Which scope the value came from (`warehouse` or `global`), when set.",
          "type": [
            "string",
            "null"
          ]
        },
        "value": {
          "type": [
            "string",
            "null"
          ]
        }
      },
      "required": [
        "key"
      ],
      "type": "object"
    }
  },
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The full configuration listing.",
  "properties": {
    "entries": {
      "items": {
        "$ref": "#/$defs/ConfigEntry"
      },
      "type": "array"
    }
  },
  "required": [
    "entries"
  ],
  "title": "ConfigList",
  "type": "object"
}
```

## `conflicts`

### `ConflictReport`

```json
{
  "$defs": {
    "Conflict": {
      "description": "One conflicted file. When the working copy carries diff3 markers, the three sides\nare content addresses (blob hashes) a resolver can fetch; otherwise `markers` is\nfalse and the sides are absent (a whole-file or binary conflict).",
      "properties": {
        "base": {
          "description": "The common ancestor's version (content address).",
          "type": [
            "string",
            "null"
          ]
        },
        "markers": {
          "type": "boolean"
        },
        "ours": {
          "description": "The current pallet's version (content address).",
          "type": [
            "string",
            "null"
          ]
        },
        "path": {
          "type": "string"
        },
        "theirs": {
          "description": "The consolidated pallet's version (content address).",
          "type": [
            "string",
            "null"
          ]
        }
      },
      "required": [
        "path",
        "markers"
      ],
      "type": "object"
    }
  },
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The conflict report: every file an unresolved consolidation left in conflict.",
  "properties": {
    "conflicts": {
      "items": {
        "$ref": "#/$defs/Conflict"
      },
      "type": "array"
    }
  },
  "required": [
    "conflicts"
  ],
  "title": "ConflictReport",
  "type": "object"
}
```

## `consolidate`

### `ConsolidateReport`

```json
{
  "$defs": {
    "ConsolidateOutcome": {
      "description": "What a consolidation did. `Conflicts` is the only outcome that leaves work for the\noperator (resolve, load, stack); the rest are complete.",
      "enum": [
        "up_to_date",
        "fast_forward",
        "merged",
        "conflicts"
      ],
      "type": "string"
    }
  },
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The result of a consolidate.",
  "properties": {
    "conflicts": {
      "description": "The conflicting paths, when the merge did not complete cleanly.",
      "items": {
        "type": "string"
      },
      "type": "array"
    },
    "outcome": {
      "$ref": "#/$defs/ConsolidateOutcome"
    },
    "pallet": {
      "description": "The pallet consolidated into (the current one).",
      "type": "string"
    },
    "parcel": {
      "description": "The merge (or fast-forward) parcel/head, when one resulted.",
      "type": [
        "string",
        "null"
      ]
    },
    "target": {
      "description": "The pallet consolidated in.",
      "type": "string"
    }
  },
  "required": [
    "outcome",
    "pallet",
    "target",
    "conflicts"
  ],
  "title": "ConsolidateReport",
  "type": "object"
}
```

## `deliver`

### `DeliverReport`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The result of a delivery.",
  "properties": {
    "checkpoints": {
      "description": "How many checkpoints were squashed.",
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    },
    "delivered": {
      "description": "The clean squashed parcel now on the target pallet.",
      "type": "string"
    },
    "manifest_head": {
      "description": "The manifest parcel that recorded the delivery.",
      "type": "string"
    },
    "source": {
      "description": "The draft pallet the trail came from (kept).",
      "type": "string"
    },
    "target": {
      "description": "The target pallet (now the current pallet).",
      "type": "string"
    },
    "trail_head": {
      "description": "The trail tip that was squashed.",
      "type": "string"
    }
  },
  "required": [
    "delivered",
    "target",
    "source",
    "trail_head",
    "checkpoints",
    "manifest_head"
  ],
  "title": "DeliverReport",
  "type": "object"
}
```

## `diff`

### `DiffReport`

```json
{
  "$defs": {
    "ChangeKind": {
      "description": "The kind of a change reported by a stocktake.",
      "oneOf": [
        {
          "const": "added",
          "description": "The item exists in the newer state but not in the older one.",
          "type": "string"
        },
        {
          "const": "modified",
          "description": "The item exists in both states with different content.",
          "type": "string"
        },
        {
          "const": "moved",
          "description": "The item was moved: it disappeared from one path and reappeared at another with\nthe same content (detected by a move-detection post-pass; the formats stay move-agnostic).",
          "type": "string"
        },
        {
          "const": "removed",
          "description": "The item exists in the older state but not in the newer one.",
          "type": "string"
        },
        {
          "const": "untracked",
          "description": "The item exists in the working directory but is not tracked by the inventory.",
          "type": "string"
        },
        {
          "const": "conflict",
          "description": "The item is in a conflict state (an unresolved consolidation).",
          "type": "string"
        }
      ]
    },
    "DiffFileSummary": {
      "description": "One changed file in a `--json` diff.",
      "properties": {
        "kind": {
          "$ref": "#/$defs/ChangeKind"
        },
        "moved_from": {
          "description": "The old path, for a moved file.",
          "type": [
            "string",
            "null"
          ]
        },
        "path": {
          "type": "string"
        }
      },
      "required": [
        "kind",
        "path"
      ],
      "type": "object"
    }
  },
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The `--json` diff: the changed-file set. The line-by-line hunks stay a human\ndisplay (a program reads content by hash when it needs it, and stays token-cheap).",
  "properties": {
    "files": {
      "items": {
        "$ref": "#/$defs/DiffFileSummary"
      },
      "type": "array"
    },
    "mode": {
      "description": "What was compared: `worktree`, `staged` or `pallets`.",
      "type": "string"
    }
  },
  "required": [
    "mode",
    "files"
  ],
  "title": "DiffReport",
  "type": "object"
}
```

## `export-git`

### `ExportReport`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The result of a git export.",
  "properties": {
    "blobs": {
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    },
    "branches": {
      "description": "The branches created (one per user pallet).",
      "items": {
        "type": "string"
      },
      "type": "array"
    },
    "commits": {
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    },
    "current": {
      "description": "The branch checked out (the current pallet).",
      "type": [
        "string",
        "null"
      ]
    },
    "path": {
      "type": "string"
    },
    "trees": {
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    }
  },
  "required": [
    "path",
    "commits",
    "trees",
    "blobs",
    "branches"
  ],
  "title": "ExportReport",
  "type": "object"
}
```

## `franchise`

### `FranchiseReport`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The result of a franchise: what was imported and which pallet was checked out.",
  "properties": {
    "adopted_anchor": {
      "description": "Whether the remote's trust anchor was adopted.",
      "type": "boolean"
    },
    "bundle_objects": {
      "description": "Objects imported from the remote's bundle, when it had one.",
      "format": "uint",
      "minimum": 0,
      "type": [
        "integer",
        "null"
      ]
    },
    "bundle_signatures": {
      "format": "uint",
      "minimum": 0,
      "type": [
        "integer",
        "null"
      ]
    },
    "directory": {
      "type": "string"
    },
    "fetched_objects": {
      "description": "Loose objects fetched for the materialized pallet.",
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    },
    "head": {
      "description": "Its head (`null` when the pallet is unborn on the remote).",
      "type": [
        "string",
        "null"
      ]
    },
    "meta_adopted": {
      "description": "The meta pallets (e.g. `@manifest`) adopted from the remote.",
      "items": {
        "type": "string"
      },
      "type": "array"
    },
    "pallet": {
      "description": "The pallet checked out.",
      "type": "string"
    },
    "remote": {
      "type": "string"
    },
    "scope": {
      "description": "The sparse fetch scope, when this was a sparse (`--only`) franchise (empty otherwise).",
      "items": {
        "type": "string"
      },
      "type": "array"
    },
    "unborn": {
      "description": "Whether the checked-out pallet started unborn.",
      "type": "boolean"
    }
  },
  "required": [
    "remote",
    "directory",
    "adopted_anchor",
    "meta_adopted",
    "pallet",
    "unborn",
    "scope",
    "fetched_objects"
  ],
  "title": "FranchiseReport",
  "type": "object"
}
```

## `haul`

### `Opened`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "properties": {
    "id": {
      "type": "string"
    },
    "source": {
      "type": "string"
    },
    "target": {
      "type": "string"
    },
    "title": {
      "type": "string"
    }
  },
  "required": [
    "id",
    "source",
    "target",
    "title"
  ],
  "title": "Opened",
  "type": "object"
}
```

### `HaulList`

```json
{
  "$defs": {
    "HaulSummary": {
      "properties": {
        "approvals": {
          "format": "uint",
          "minimum": 0,
          "type": "integer"
        },
        "id": {
          "type": "string"
        },
        "source": {
          "type": "string"
        },
        "status": {
          "type": "string"
        },
        "target": {
          "type": "string"
        },
        "title": {
          "type": "string"
        }
      },
      "required": [
        "id",
        "title",
        "source",
        "target",
        "status",
        "approvals"
      ],
      "type": "object"
    }
  },
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "properties": {
    "hauls": {
      "items": {
        "$ref": "#/$defs/HaulSummary"
      },
      "type": "array"
    },
    "state": {
      "type": "string"
    }
  },
  "required": [
    "state",
    "hauls"
  ],
  "title": "HaulList",
  "type": "object"
}
```

### `HaulDetail`

```json
{
  "$defs": {
    "ReviewLine": {
      "properties": {
        "author": {
          "type": "string"
        },
        "body": {
          "type": "string"
        },
        "class": {
          "type": [
            "string",
            "null"
          ]
        },
        "verdict": {
          "type": "string"
        }
      },
      "required": [
        "author",
        "verdict",
        "body"
      ],
      "type": "object"
    },
    "ThreadLine": {
      "properties": {
        "author": {
          "type": "string"
        },
        "body": {
          "type": "string"
        },
        "kind": {
          "type": "string"
        }
      },
      "required": [
        "author",
        "kind",
        "body"
      ],
      "type": "object"
    }
  },
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "properties": {
    "description": {
      "type": "string"
    },
    "head": {
      "type": "string"
    },
    "id": {
      "type": "string"
    },
    "opened_by": {
      "type": "string"
    },
    "reviews": {
      "items": {
        "$ref": "#/$defs/ReviewLine"
      },
      "type": "array"
    },
    "source": {
      "type": "string"
    },
    "status": {
      "type": "string"
    },
    "target": {
      "type": "string"
    },
    "thread": {
      "items": {
        "$ref": "#/$defs/ThreadLine"
      },
      "type": "array"
    },
    "title": {
      "type": "string"
    }
  },
  "required": [
    "id",
    "title",
    "source",
    "target",
    "status",
    "head",
    "opened_by",
    "description",
    "reviews",
    "thread"
  ],
  "title": "HaulDetail",
  "type": "object"
}
```

### `Acted`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "properties": {
    "action": {
      "type": "string"
    },
    "id": {
      "type": "string"
    }
  },
  "required": [
    "id",
    "action"
  ],
  "title": "Acted",
  "type": "object"
}
```

### `Merged`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "properties": {
    "already": {
      "type": "boolean"
    },
    "id": {
      "type": "string"
    },
    "merge_parcel": {
      "type": "string"
    }
  },
  "required": [
    "id",
    "merge_parcel",
    "already"
  ],
  "title": "Merged",
  "type": "object"
}
```

### `MergeConflicts`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "properties": {
    "conflicts": {
      "items": {
        "type": "string"
      },
      "type": "array"
    },
    "id": {
      "type": "string"
    }
  },
  "required": [
    "id",
    "conflicts"
  ],
  "title": "MergeConflicts",
  "type": "object"
}
```

## `heal`

### `HealReport`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The result of a `heal` run that fully resolved (or found nothing to resolve).",
  "properties": {
    "notes": {
      "description": "Advisory notes that never blocked clearing: the \"re-run the load\" remedy for each vanished\ninventory shard in `resolved`, and a note for each bay whose local state could not be\nread this run (skipped rather than blocking recovery) naming the bay and how to clean it up.",
      "items": {
        "type": "string"
      },
      "type": "array"
    },
    "resolved": {
      "description": "Recorded paths (or hashes) resolved without a rewrite: proven absent and unreferenced by\nthe closure walk, or a vanished inventory shard (a staging concern, never an object-trust\none — see `notes`).",
      "items": {
        "type": "string"
      },
      "type": "array"
    },
    "restaged": {
      "description": "Recorded paths that were present, verified, and freshly rewritten.",
      "items": {
        "type": "string"
      },
      "type": "array"
    },
    "was_tainted": {
      "description": "Whether a durability taint was actually standing (an already-healthy warehouse reports\n`false`, with every other list empty).",
      "type": "boolean"
    }
  },
  "required": [
    "was_tainted",
    "restaged",
    "resolved",
    "notes"
  ],
  "title": "HealReport",
  "type": "object"
}
```

## `history`

### `History`

```json
{
  "$defs": {
    "HistoryAction": {
      "description": "One authorship/stack action within a parcel.",
      "properties": {
        "action": {
          "type": "string"
        },
        "class": {
          "description": "The operator's identity class, when it is not a plain human — so agent,\nbot and service authorship is legible in the log.",
          "type": [
            "string",
            "null"
          ]
        },
        "name": {
          "description": "The resolved display name, when a resolution hook supplied one.",
          "type": [
            "string",
            "null"
          ]
        },
        "operator": {
          "description": "The pseudonymous operator id (always present — it is what the chain records).",
          "type": "string"
        },
        "supervisor": {
          "description": "The supervising human of an automated identity, when one is recorded.",
          "type": [
            "string",
            "null"
          ]
        },
        "timestamp": {
          "description": "The action time. Serialized as RFC 3339 (UTC) for `--json`; formatted directly for\nthe human log, so no timestamp is ever converted to a string and parsed back.",
          "format": "date-time",
          "type": "string"
        },
        "trust": {
          "description": "Always \"recorded\": history reads the identity the parcel itself records (and joins\nthe office off it) without verifying the signature. `query --class` is the verified\ncounterpart — same flag word, different trust tier, stamped here so no consumer can\nmistake one for the other.",
          "type": "string"
        }
      },
      "required": [
        "action",
        "operator",
        "trust",
        "timestamp"
      ],
      "type": "object"
    },
    "HistoryEntry": {
      "description": "One parcel in the history.",
      "properties": {
        "actions": {
          "items": {
            "$ref": "#/$defs/HistoryAction"
          },
          "type": "array"
        },
        "consolidates": {
          "description": "The parents a consolidation merges (present only for merge parcels).",
          "items": {
            "type": "string"
          },
          "type": "array"
        },
        "description": {
          "type": [
            "string",
            "null"
          ]
        },
        "parcel": {
          "type": "string"
        },
        "parents": {
          "description": "This parcel's parents, in their stored (canonical, base-first) order — always present,\n`[]` for a root parcel. Unlike `consolidates` (kept for compatibility, only non-empty on\na merge), this is the graph edge a caller building a DAG needs regardless of parcel kind.",
          "items": {
            "type": "string"
          },
          "type": "array"
        }
      },
      "required": [
        "parcel",
        "consolidates",
        "parents",
        "actions"
      ],
      "type": "object"
    }
  },
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The parcel history: parcels newest first.",
  "properties": {
    "entries": {
      "items": {
        "$ref": "#/$defs/HistoryEntry"
      },
      "type": "array"
    },
    "next": {
      "description": "The cursor for the next `--json` page: pass it back as `--after` to resume. Absent\nonce the history is exhausted. (Only meaningful with `-n`/`--limit`.)",
      "type": [
        "string",
        "null"
      ]
    }
  },
  "required": [
    "entries"
  ],
  "title": "History",
  "type": "object"
}
```

## `import-git`

### `ImportReport`

```json
{
  "$defs": {
    "Packed": {
      "description": "The packed-store summary (a slim view of `pack_utils::IngestStats`).",
      "properties": {
        "deltas": {
          "description": "Of the objects, blobs delta-compressed against the previous version at their path.",
          "format": "uint",
          "minimum": 0,
          "type": "integer"
        },
        "objects": {
          "description": "Objects written into packs.",
          "format": "uint",
          "minimum": 0,
          "type": "integer"
        },
        "packs": {
          "description": "Packs written.",
          "format": "uint",
          "minimum": 0,
          "type": "integer"
        }
      },
      "required": [
        "objects",
        "packs",
        "deltas"
      ],
      "type": "object"
    }
  },
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The result of a git import.",
  "properties": {
    "blobs": {
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    },
    "commits": {
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    },
    "compacted": {
      "anyOf": [
        {
          "$ref": "#/$defs/Packed"
        },
        {
          "type": "null"
        }
      ],
      "description": "What the pack-direct import wrote (absent with `--no-compact`). The field keeps its\noriginal name: it reports the same fact — the imported store is packed — that the old\npost-import compaction pass did."
    },
    "current": {
      "description": "The pallet checked out (git's HEAD branch).",
      "type": [
        "string",
        "null"
      ]
    },
    "ignored_git": {
      "description": "Whether a `.git` ignore pattern was added to `.forkliftignore`.",
      "type": "boolean"
    },
    "pallets": {
      "description": "The pallets created (one per local branch).",
      "items": {
        "type": "string"
      },
      "type": "array"
    },
    "trees": {
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    },
    "warnings": {
      "description": "Anything skipped or worth flagging (e.g. submodules).",
      "items": {
        "type": "string"
      },
      "type": "array"
    }
  },
  "required": [
    "commits",
    "trees",
    "blobs",
    "pallets",
    "ignored_git",
    "warnings"
  ],
  "title": "ImportReport",
  "type": "object"
}
```

## `lift`

### `LiftReport`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The result of a lift: the pallet's outcome, plus the office lift when trust\nrequired its keys to reach the remote first.",
  "properties": {
    "auto_merged": {
      "description": "How many times the lift auto-merged a diverged remote before it went through\n(optimistic lift, §7.7).",
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    },
    "head": {
      "type": "string"
    },
    "meta_pallets": {
      "description": "The meta pallets (e.g. `@manifest`) lifted with new parcels.",
      "items": {
        "type": "string"
      },
      "type": "array"
    },
    "new_parcels": {
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    },
    "office_new_parcels": {
      "description": "The parcels the office lift uploaded, when one happened.",
      "format": "uint",
      "minimum": 0,
      "type": [
        "integer",
        "null"
      ]
    },
    "pallet": {
      "type": "string"
    },
    "up_to_date": {
      "description": "Whether the remote already had the local head.",
      "type": "boolean"
    },
    "uploaded_objects": {
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    },
    "uploaded_signatures": {
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    }
  },
  "required": [
    "meta_pallets",
    "auto_merged",
    "pallet",
    "up_to_date",
    "head",
    "new_parcels",
    "uploaded_objects",
    "uploaded_signatures"
  ],
  "title": "LiftReport",
  "type": "object"
}
```

## `load`

### `Loaded`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The path a `load` staged. Human output stays silent (as it always has); `--json`\nstill gets a confirmation envelope so a program sees a result.",
  "properties": {
    "path": {
      "type": "string"
    }
  },
  "required": [
    "path"
  ],
  "title": "Loaded",
  "type": "object"
}
```

## `lower`

### `LowerReport`

```json
{
  "$defs": {
    "LowerOutcome": {
      "description": "What a lower did to the current pallet.",
      "oneOf": [
        {
          "const": "up_to_date",
          "description": "The local pallet was already at the remote head.",
          "type": "string"
        },
        {
          "const": "ahead",
          "description": "The local pallet is ahead of the remote (lift to publish).",
          "type": "string"
        },
        {
          "const": "lowered",
          "description": "The local pallet fast-forwarded to the remote head.",
          "type": "string"
        }
      ]
    }
  },
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The result of a lower: the trust sync that ran first, then the pallet's outcome.",
  "properties": {
    "adopted_anchor": {
      "description": "Whether the remote's trust anchor was adopted on first contact.",
      "type": "boolean"
    },
    "fetched_objects": {
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    },
    "fetched_signatures": {
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    },
    "head": {
      "type": "string"
    },
    "meta_adopted": {
      "description": "Meta pallets (e.g. `@manifest`) fast-forwarded or adopted from the remote.",
      "items": {
        "type": "string"
      },
      "type": "array"
    },
    "meta_merged": {
      "description": "Meta pallets whose diverged history was merged with the remote's.",
      "items": {
        "type": "string"
      },
      "type": "array"
    },
    "office_moved": {
      "description": "Whether the office pallet moved with the remote.",
      "type": "boolean"
    },
    "outcome": {
      "$ref": "#/$defs/LowerOutcome"
    },
    "pallet": {
      "type": "string"
    }
  },
  "required": [
    "adopted_anchor",
    "office_moved",
    "meta_adopted",
    "meta_merged",
    "pallet",
    "outcome",
    "head",
    "fetched_objects",
    "fetched_signatures"
  ],
  "title": "LowerReport",
  "type": "object"
}
```

## `manifest`

### `Recorded`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "A newly recorded manifest entry.",
  "properties": {
    "body": {
      "type": "string"
    },
    "kind": {
      "type": "string"
    },
    "manifest_head": {
      "description": "The new head of the manifest pallet.",
      "type": "string"
    },
    "operator": {
      "type": "string"
    },
    "subject": {
      "type": "string"
    }
  },
  "required": [
    "kind",
    "subject",
    "operator",
    "body",
    "manifest_head"
  ],
  "title": "Recorded",
  "type": "object"
}
```

### `ManifestView`

```json
{
  "$defs": {
    "EntryView": {
      "description": "One manifest entry in a view, with its forge-proof author.",
      "properties": {
        "author": {
          "type": "string"
        },
        "body": {
          "type": "string"
        },
        "checkpoints": {
          "format": "int64",
          "type": [
            "integer",
            "null"
          ]
        },
        "kind": {
          "type": "string"
        },
        "model": {
          "description": "Provenance fields, present only on a provenance entry.",
          "type": [
            "string",
            "null"
          ]
        },
        "recorded_at": {
          "format": "int64",
          "type": "integer"
        },
        "session": {
          "type": [
            "string",
            "null"
          ]
        },
        "source": {
          "description": "Delivery fields, present only on a delivery entry.",
          "type": [
            "string",
            "null"
          ]
        },
        "subject": {
          "type": "string"
        },
        "tool": {
          "type": [
            "string",
            "null"
          ]
        },
        "trail_head": {
          "type": [
            "string",
            "null"
          ]
        },
        "transcript": {
          "type": [
            "string",
            "null"
          ]
        }
      },
      "required": [
        "kind",
        "subject",
        "author",
        "recorded_at",
        "body"
      ],
      "type": "object"
    }
  },
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The manifest, or the slice of it about one parcel.",
  "properties": {
    "entries": {
      "items": {
        "$ref": "#/$defs/EntryView"
      },
      "type": "array"
    },
    "subject": {
      "description": "The parcel the view is scoped to (`null` when listing the whole manifest).",
      "type": [
        "string",
        "null"
      ]
    }
  },
  "required": [
    "entries"
  ],
  "title": "ManifestView",
  "type": "object"
}
```

## `office`

### `OfficeListing`

```json
{
  "$defs": {
    "OfficeKey": {
      "description": "One key of an operator.",
      "properties": {
        "identity_root": {
          "description": "Whether this is the operator's pinned identity root.",
          "type": "boolean"
        },
        "key_id": {
          "type": "string"
        },
        "on_this_machine": {
          "description": "Whether the private half is present on this machine (an active key you can sign with).",
          "type": "boolean"
        },
        "protected": {
          "description": "Whether the local private key is passphrase-protected (encrypted at rest).",
          "type": "boolean"
        },
        "retired": {
          "type": "boolean"
        }
      },
      "required": [
        "key_id",
        "retired",
        "on_this_machine",
        "protected",
        "identity_root"
      ],
      "type": "object"
    },
    "OfficeUser": {
      "description": "One enrolled operator.",
      "properties": {
        "class": {
          "description": "The identity class: human / agent / bot / service.",
          "type": "string"
        },
        "identifier": {
          "type": "string"
        },
        "keys": {
          "items": {
            "$ref": "#/$defs/OfficeKey"
          },
          "type": "array"
        },
        "name": {
          "description": "The resolved display name, when a resolution hook supplied one.",
          "type": [
            "string",
            "null"
          ]
        },
        "pallets": {
          "description": "The pallets a writer is restricted to (empty = all pallets).",
          "items": {
            "type": "string"
          },
          "type": "array"
        },
        "role": {
          "type": "string"
        },
        "supervisor": {
          "description": "The supervising human, for an automated identity.",
          "type": [
            "string",
            "null"
          ]
        }
      },
      "required": [
        "identifier",
        "role",
        "class",
        "pallets",
        "keys"
      ],
      "type": "object"
    }
  },
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The office roster: users, their roles/grants, and their keys.",
  "properties": {
    "enrolled": {
      "description": "Whether trust is established (anyone is enrolled).",
      "type": "boolean"
    },
    "users": {
      "items": {
        "$ref": "#/$defs/OfficeUser"
      },
      "type": "array"
    }
  },
  "required": [
    "enrolled",
    "users"
  ],
  "title": "OfficeListing",
  "type": "object"
}
```

## `palletize`

### `Palletized`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "A newly created pallet.",
  "properties": {
    "at_revision": {
      "description": "The revision the pallet was created at, when one was given (otherwise it shares\nthe current pallet's head).",
      "type": [
        "string",
        "null"
      ]
    },
    "head": {
      "description": "The head the new pallet points at (`null` when created unborn from an unborn\ncurrent pallet).",
      "type": [
        "string",
        "null"
      ]
    },
    "pallet": {
      "type": "string"
    }
  },
  "required": [
    "pallet"
  ],
  "title": "Palletized",
  "type": "object"
}
```

### `PalletList`

```json
{
  "$defs": {
    "PalletEntry": {
      "description": "One pallet in the list.",
      "properties": {
        "current": {
          "type": "boolean"
        },
        "head": {
          "description": "The pallet's head parcel hash; `null` when it is unborn (nothing stacked on it yet).",
          "type": [
            "string",
            "null"
          ]
        },
        "name": {
          "type": "string"
        }
      },
      "required": [
        "name",
        "current"
      ],
      "type": "object"
    }
  },
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The list of pallets, marking the current one.",
  "properties": {
    "current": {
      "description": "The current pallet (HEAD equivalent).",
      "type": "string"
    },
    "current_unborn": {
      "description": "Whether the current pallet is unborn (no parcel stacked on it yet).",
      "type": "boolean"
    },
    "meta": {
      "description": "The meta pallets in their qualified form (`@office`), present only when `--all`\nwas given.",
      "items": {
        "type": "string"
      },
      "type": "array"
    },
    "pallets": {
      "description": "Every user pallet with a ref file.",
      "items": {
        "$ref": "#/$defs/PalletEntry"
      },
      "type": "array"
    }
  },
  "required": [
    "current",
    "current_unborn",
    "pallets",
    "meta"
  ],
  "title": "PalletList",
  "type": "object"
}
```

## `park`

### `ParkedList`

```json
{
  "$defs": {
    "ParkedEntry": {
      "description": "One parked parcel.",
      "properties": {
        "description": {
          "type": "string"
        },
        "parcel": {
          "type": "string"
        }
      },
      "required": [
        "parcel",
        "description"
      ],
      "type": "object"
    }
  },
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The list of parked parcels, newest first.",
  "properties": {
    "parked": {
      "items": {
        "$ref": "#/$defs/ParkedEntry"
      },
      "type": "array"
    }
  },
  "required": [
    "parked"
  ],
  "title": "ParkedList",
  "type": "object"
}
```

## `peek`

### `PeekInventory`

```json
{
  "$defs": {
    "PeekInventoryItem": {
      "description": "One inventory entry.",
      "properties": {
        "hash": {
          "type": "string"
        },
        "name": {
          "type": "string"
        },
        "state": {
          "type": "string"
        }
      },
      "required": [
        "state",
        "hash",
        "name"
      ],
      "type": "object"
    }
  },
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "A `--json` inventory peek.",
  "properties": {
    "items": {
      "items": {
        "$ref": "#/$defs/PeekInventoryItem"
      },
      "type": "array"
    }
  },
  "required": [
    "items"
  ],
  "title": "PeekInventory",
  "type": "object"
}
```

### `PeekObject`

```json
{
  "$defs": {
    "PeekAction": {
      "description": "One action of a parcel object.",
      "properties": {
        "action": {
          "type": "string"
        },
        "description": {
          "type": [
            "string",
            "null"
          ]
        },
        "operator": {
          "type": "string"
        },
        "timestamp": {
          "type": "string"
        }
      },
      "required": [
        "action",
        "operator",
        "timestamp"
      ],
      "type": "object"
    },
    "PeekChunk": {
      "description": "One chunk of a recipe object.",
      "properties": {
        "hash": {
          "type": "string"
        },
        "size": {
          "format": "uint64",
          "minimum": 0,
          "type": "integer"
        }
      },
      "required": [
        "hash",
        "size"
      ],
      "type": "object"
    },
    "PeekTreeEntry": {
      "description": "One entry of a tree object.",
      "properties": {
        "hash": {
          "type": "string"
        },
        "item_type": {
          "type": "string"
        },
        "name": {
          "type": "string"
        }
      },
      "required": [
        "item_type",
        "hash",
        "name"
      ],
      "type": "object"
    }
  },
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "A `--json` object peek: the fields relevant to the object's type are set.",
  "properties": {
    "actions": {
      "description": "A parcel's actions.",
      "items": {
        "$ref": "#/$defs/PeekAction"
      },
      "type": "array"
    },
    "binary": {
      "description": "Whether a blob is binary — contains a NUL byte, or is not valid UTF-8 (see\n`output::blob_text`) — `content` is then omitted rather than carrying lossily-mangled\nbytes. Absent for every other object type.",
      "type": [
        "boolean",
        "null"
      ]
    },
    "chunks": {
      "description": "A recipe's ordered chunk list.",
      "items": {
        "$ref": "#/$defs/PeekChunk"
      },
      "type": "array"
    },
    "content": {
      "description": "A blob's content as text.",
      "type": [
        "string",
        "null"
      ]
    },
    "content_hash": {
      "description": "A recipe's whole-file content hash.",
      "type": [
        "string",
        "null"
      ]
    },
    "description": {
      "description": "A parcel's description.",
      "type": [
        "string",
        "null"
      ]
    },
    "entries": {
      "description": "A tree's entries.",
      "items": {
        "$ref": "#/$defs/PeekTreeEntry"
      },
      "type": "array"
    },
    "object_type": {
      "type": "string"
    },
    "parents": {
      "description": "A parcel's parent hashes.",
      "items": {
        "type": "string"
      },
      "type": "array"
    },
    "total_size": {
      "description": "A recipe's total assembled size, or a chunk's payload size.",
      "format": "uint64",
      "minimum": 0,
      "type": [
        "integer",
        "null"
      ]
    },
    "tree": {
      "description": "A parcel's root tree hash.",
      "type": [
        "string",
        "null"
      ]
    }
  },
  "required": [
    "object_type",
    "entries",
    "parents",
    "actions",
    "chunks"
  ],
  "title": "PeekObject",
  "type": "object"
}
```

## `prepare`

### `Prepared`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The pieces `prepare` created (empty when the warehouse already existed).",
  "properties": {
    "created": {
      "items": {
        "type": "string"
      },
      "type": "array"
    }
  },
  "required": [
    "created"
  ],
  "title": "Prepared",
  "type": "object"
}
```

## `profile`

### `ProfileList`

```json
{
  "$defs": {
    "ProfileEntry": {
      "description": "One profile and how many local keys it holds.",
      "properties": {
        "display_name": {
          "type": [
            "string",
            "null"
          ]
        },
        "identifier": {
          "description": "The operator id (`null` for the default before any id is minted).",
          "type": [
            "string",
            "null"
          ]
        },
        "local_keys": {
          "format": "uint",
          "minimum": 0,
          "type": "integer"
        },
        "name": {
          "type": "string"
        }
      },
      "required": [
        "name",
        "local_keys"
      ],
      "type": "object"
    }
  },
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The profile listing: the default identity and every named profile.",
  "properties": {
    "default": {
      "$ref": "#/$defs/ProfileEntry"
    },
    "profiles": {
      "items": {
        "$ref": "#/$defs/ProfileEntry"
      },
      "type": "array"
    }
  },
  "required": [
    "default",
    "profiles"
  ],
  "title": "ProfileList",
  "type": "object"
}
```

## `query`

### `QueryReport`

```json
{
  "$defs": {
    "QueryAction": {
      "description": "One recorded authorship/stack action (history-shaped, explicitly labeled recorded).",
      "properties": {
        "action": {
          "type": "string"
        },
        "class": {
          "type": [
            "string",
            "null"
          ]
        },
        "operator": {
          "description": "The pseudonymous operator id the parcel records (self-declared).",
          "type": "string"
        },
        "supervisor": {
          "type": [
            "string",
            "null"
          ]
        },
        "timestamp": {
          "format": "date-time",
          "type": "string"
        },
        "trust": {
          "description": "Always \"recorded\": a per-action identity is the parcel's own claim. The parcel-level\n`author` block carries the verified resolution.",
          "type": "string"
        }
      },
      "required": [
        "action",
        "operator",
        "trust",
        "timestamp"
      ],
      "type": "object"
    },
    "QueryEntry": {
      "description": "One matching parcel.",
      "properties": {
        "actions": {
          "description": "The recorded per-action history (always the parcel's own claim, so each action is\nlabeled trust \"recorded\" — the parcel-level `author` is where verification lives).",
          "items": {
            "$ref": "#/$defs/QueryAction"
          },
          "type": "array"
        },
        "author": {
          "$ref": "#/$defs/QueryIdentity",
          "description": "The resolved author identity at the query's trust level. Under verified trust this\nis the verified signer (the only forge-proof attribution a parcel has); under\nrecorded trust, the parcel's own claim."
        },
        "description": {
          "type": [
            "string",
            "null"
          ]
        },
        "is_merge": {
          "type": "boolean"
        },
        "parcel": {
          "type": "string"
        },
        "provenance": {
          "anyOf": [
            {
              "$ref": "#/$defs/QueryProvenance"
            },
            {
              "type": "null"
            }
          ],
          "description": "This subject's newest machine-authorship provenance entry, if `@manifest` has any.\nAbsent both when there is no entry for this parcel and when the whole pallet has no\nhead — `scope.provenance_source` is what tells those two apart."
        },
        "signer": {
          "anyOf": [
            {
              "$ref": "#/$defs/QuerySigner"
            },
            {
              "type": "null"
            }
          ],
          "description": "The verified signer, when the parcel carries a signature that verifies."
        },
        "tags": {
          "description": "This subject's tag names (omitted, not empty-listed, when it carries none).",
          "items": {
            "type": "string"
          },
          "type": "array"
        }
      },
      "required": [
        "parcel",
        "author",
        "is_merge",
        "actions",
        "tags"
      ],
      "type": "object"
    },
    "QueryIdentity": {
      "description": "A resolved identity: operator, office class/supervisor, and the trust of the resolution.",
      "properties": {
        "class": {
          "type": [
            "string",
            "null"
          ]
        },
        "operator": {
          "description": "The resolved operator id; absent when nothing resolves (unsigned / unknown key).",
          "type": [
            "string",
            "null"
          ]
        },
        "supervisor": {
          "type": [
            "string",
            "null"
          ]
        },
        "trust": {
          "description": "\"verified\" | \"signed-revoked\" | \"unsigned\" | \"unknown-key\" | \"recorded\".",
          "type": "string"
        }
      },
      "required": [
        "trust"
      ],
      "type": "object"
    },
    "QueryProvenance": {
      "description": "The machine-authorship provenance a match carries, flattened for the report.",
      "properties": {
        "model": {
          "type": "string"
        },
        "session": {
          "type": [
            "string",
            "null"
          ]
        },
        "tool": {
          "type": [
            "string",
            "null"
          ]
        }
      },
      "required": [
        "model"
      ],
      "type": "object"
    },
    "QueryScope": {
      "description": "The honesty block: what the walk covered, at what trust, and what it could not see.",
      "properties": {
        "fetch_scope": {
          "description": "The warehouse's fetch-scope prefixes — present only on a sparse warehouse.",
          "items": {
            "type": "string"
          },
          "type": [
            "array",
            "null"
          ]
        },
        "matched": {
          "description": "How many parcels matched.",
          "format": "uint",
          "minimum": 0,
          "type": "integer"
        },
        "office_asof": {
          "description": "Office reads are a current snapshot: class/supervisor answers are \"as recorded in\nthe office today\", not as of each parcel's authoring time. Always \"current\".",
          "type": "string"
        },
        "out_of_scope": {
          "description": "Parcels a `touches` predicate could not confirm because the path was provably\noutside a sparse warehouse's fetch scope (degraded to `Unknown`, not an error). 0\noutside that case.",
          "format": "uint",
          "minimum": 0,
          "type": "integer"
        },
        "provenance_source": {
          "description": "Whether the `@manifest` meta pallet has a head at all: `\"present\"`, or\n`\"meta_pallet_absent\"` when it does not exist (or was never fetched) — every\nprovenance leaf then reads `Unknown` for lack of a pallet to consult, not for lack\nof evidence on any one parcel.",
          "type": "string"
        },
        "tags_source": {
          "description": "Whether the `@tags` meta pallet has a head at all: `\"present\"`, or\n`\"meta_pallet_absent\"` when it does not exist (or was never fetched) — mirrors\n`provenance_source`. A match's omitted `tags` only proves \"genuinely untagged\" when\nthis reads `\"present\"`; when it reads `\"meta_pallet_absent\"` every `tags` omission is\nunknowable, not a negative result.",
          "type": "string"
        },
        "trust": {
          "description": "\"verified\" or \"recorded\" — the trust level identity answers were resolved at.",
          "type": "string"
        },
        "walked": {
          "description": "How many parcels the walk considered.",
          "format": "uint",
          "minimum": 0,
          "type": "integer"
        }
      },
      "required": [
        "trust",
        "office_asof",
        "walked",
        "matched",
        "out_of_scope",
        "provenance_source",
        "tags_source"
      ],
      "type": "object"
    },
    "QuerySigner": {
      "description": "The verified signing key and its office binding.",
      "properties": {
        "boundary": {
          "description": "\"vouched\" | \"suspect\" | \"unresolved\": whether this signed-revoked match sits inside\nthe revoking key's distrust boundary (the history the revoker vouched for at\nrevocation time) or outside it. \"suspect\" means a forged backdate, or the key's\nholder kept signing after the revocation — `audit` refuses such a warehouse outright;\na read-only query cannot refuse a signed history it was only asked to read, so this\nis the loud label instead. \"unresolved\" means the question cannot be answered on\n*this* store: at least one of the revocation's boundary heads was never fetched here\n(a partial clone that only ever pulled reachable history) — never treat this as\n\"suspect\"; query the origin, or fetch the full history, for a definitive answer.\nPresent exactly when the match is signed-revoked.",
          "type": [
            "string",
            "null"
          ]
        },
        "class": {
          "type": [
            "string",
            "null"
          ]
        },
        "key": {
          "type": "string"
        },
        "operator": {
          "type": [
            "string",
            "null"
          ]
        },
        "revocation_reason": {
          "description": "Why the signing key was revoked — present exactly when the match is signed-revoked.",
          "type": [
            "string",
            "null"
          ]
        }
      },
      "required": [
        "key"
      ],
      "type": "object"
    }
  },
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The query result: the matching parcels plus the honesty scope block.",
  "properties": {
    "matches": {
      "items": {
        "$ref": "#/$defs/QueryEntry"
      },
      "type": "array"
    },
    "next": {
      "description": "The cursor for the next `--json` page: pass it back as `--after` to resume. Absent\nonce the history is exhausted. (Only meaningful with `-n`/`--limit`.)",
      "type": [
        "string",
        "null"
      ]
    },
    "scope": {
      "$ref": "#/$defs/QueryScope",
      "description": "What this pass covered and at what trust — always present, so a consumer can never\nmistake a partial or unverified pass for a complete, verified one."
    }
  },
  "required": [
    "matches",
    "scope"
  ],
  "title": "QueryReport",
  "type": "object"
}
```

## `remove`

### `Removed`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The path a `remove` staged for removal. Human output stays silent; `--json` gets\na confirmation envelope.",
  "properties": {
    "path": {
      "type": "string"
    }
  },
  "required": [
    "path"
  ],
  "title": "Removed",
  "type": "object"
}
```

## `self-update`

### `SelfUpdate`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The result of a self-update check (or update).",
  "properties": {
    "applied": {
      "description": "Whether this run actually applied the update.",
      "type": "boolean"
    },
    "current": {
      "type": "string"
    },
    "install_method": {
      "description": "How this binary was installed (`cargo`, `homebrew`, `script`).",
      "type": "string"
    },
    "latest": {
      "description": "The latest published release, or `None` if there are none yet / GitHub was silent.",
      "type": [
        "string",
        "null"
      ]
    },
    "update_available": {
      "type": "boolean"
    },
    "update_command": {
      "description": "The command that updates a binary installed this way.",
      "type": "string"
    }
  },
  "required": [
    "current",
    "update_available",
    "install_method",
    "update_command",
    "applied"
  ],
  "title": "SelfUpdate",
  "type": "object"
}
```

## `shift`

### `Shifted`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The pallet a `shift` moved to and its head.",
  "properties": {
    "head": {
      "type": "string"
    },
    "pallet": {
      "type": "string"
    }
  },
  "required": [
    "pallet",
    "head"
  ],
  "title": "Shifted",
  "type": "object"
}
```

## `show`

### `Shown`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "A `show` result: a file's content at a revision, or — when it is binary or a chunked\nlarge file — the metadata that explains why there is no `content` instead. The public\nJSON schema (a change here is a schema change; see `crate::output::SCHEMA_VERSION`).",
  "properties": {
    "binary": {
      "description": "Whether the content is not shown as text: either non-text bytes (a NUL byte anywhere,\nor invalid UTF-8 — see `output::blob_text`) or a chunked large file, which is never\nassembled just to answer `show`.",
      "type": "boolean"
    },
    "chunk_count": {
      "description": "A chunked file's chunk count. Present only for a chunked file.",
      "format": "uint",
      "minimum": 0,
      "type": [
        "integer",
        "null"
      ]
    },
    "content": {
      "description": "The file's content as text. Present only when `binary` is `false`.",
      "type": [
        "string",
        "null"
      ]
    },
    "content_hash": {
      "description": "A chunked file's whole-content hash (advisory until assembly; see [`object_utils`]'s\n`Recipe`). Present only for a chunked file.",
      "type": [
        "string",
        "null"
      ]
    },
    "hash": {
      "description": "The tree entry's own object hash: a blob hash for plain content, a recipe hash for a\nchunked large file.",
      "type": "string"
    },
    "path": {
      "description": "The path, as given (already validated to exist in the revision's tree).",
      "type": "string"
    },
    "revision": {
      "description": "The resolved parcel hash the revision argument named (a pallet head, a meta-pallet\nhead, or the parcel a hash prefix matched) — never the raw revision argument, so a\ncaller always gets the exact, disambiguated parcel this content came from.",
      "type": "string"
    },
    "size": {
      "description": "The file's size in bytes: the blob length, or a chunked file's assembled total size.",
      "format": "uint64",
      "minimum": 0,
      "type": "integer"
    }
  },
  "required": [
    "revision",
    "path",
    "hash",
    "binary",
    "size"
  ],
  "title": "Shown",
  "type": "object"
}
```

## `stack`

### `Stacked`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The parcel a `stack` created and the pallet it advanced.",
  "properties": {
    "pallet": {
      "type": "string"
    },
    "parcel": {
      "type": "string"
    }
  },
  "required": [
    "parcel",
    "pallet"
  ],
  "title": "Stacked",
  "type": "object"
}
```

## `stocktake`

### `StocktakeReport`

```json
{
  "$defs": {
    "Change": {
      "description": "A single change reported by a stocktake.",
      "properties": {
        "kind": {
          "$ref": "#/$defs/ChangeKind"
        },
        "moved_from": {
          "description": "The old path of a `Moved` item; `None` for every other kind.",
          "type": [
            "string",
            "null"
          ]
        },
        "path": {
          "description": "The warehouse path of the changed item (`/`-separated, relative to the root).\nUntracked directories are reported with a trailing `/` and are not descended into.\nFor `Moved` items this is the new path.",
          "type": "string"
        }
      },
      "required": [
        "kind",
        "path"
      ],
      "type": "object"
    },
    "ChangeKind": {
      "description": "The kind of a change reported by a stocktake.",
      "oneOf": [
        {
          "const": "added",
          "description": "The item exists in the newer state but not in the older one.",
          "type": "string"
        },
        {
          "const": "modified",
          "description": "The item exists in both states with different content.",
          "type": "string"
        },
        {
          "const": "moved",
          "description": "The item was moved: it disappeared from one path and reappeared at another with\nthe same content (detected by a move-detection post-pass; the formats stay move-agnostic).",
          "type": "string"
        },
        {
          "const": "removed",
          "description": "The item exists in the older state but not in the newer one.",
          "type": "string"
        },
        {
          "const": "untracked",
          "description": "The item exists in the working directory but is not tracked by the inventory.",
          "type": "string"
        },
        {
          "const": "conflict",
          "description": "The item is in a conflict state (an unresolved consolidation).",
          "type": "string"
        }
      ]
    }
  },
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The stocktake report: the current pallet's state plus the staged and unstaged\nchanges (counts always; per-path lists unless `--summary`).",
  "properties": {
    "consolidation_in_progress": {
      "description": "The pallet being consolidated in, when a merge is in progress.",
      "type": [
        "string",
        "null"
      ]
    },
    "head": {
      "description": "The pallet's head parcel, or `null` when it is unborn.",
      "type": [
        "string",
        "null"
      ]
    },
    "pallet": {
      "description": "The current pallet.",
      "type": "string"
    },
    "staged": {
      "description": "The staged changes (empty under `--summary`).",
      "items": {
        "$ref": "#/$defs/Change"
      },
      "type": "array"
    },
    "staged_count": {
      "description": "How many changes are staged (inventory vs pallet head).",
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    },
    "summary": {
      "description": "Whether this is a counts-only report.",
      "type": "boolean"
    },
    "unstaged": {
      "description": "The unstaged changes (empty under `--summary`).",
      "items": {
        "$ref": "#/$defs/Change"
      },
      "type": "array"
    },
    "unstaged_count": {
      "description": "How many changes are unstaged (working directory vs inventory).",
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    }
  },
  "required": [
    "pallet",
    "staged_count",
    "unstaged_count",
    "staged",
    "unstaged",
    "summary"
  ],
  "title": "StocktakeReport",
  "type": "object"
}
```

## `expand`

### `ExpandReport`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The result of an expand: what was added to the fetch scope and how much was fetched.",
  "properties": {
    "added": {
      "description": "The prefixes newly added to the fetch scope (empty when everything was already in scope).",
      "items": {
        "type": "string"
      },
      "type": "array"
    },
    "fetched_objects": {
      "description": "Loose objects fetched for the newly in-scope subtree(s).",
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    },
    "scope": {
      "description": "The warehouse fetch scope after the expand.",
      "items": {
        "type": "string"
      },
      "type": "array"
    }
  },
  "required": [
    "added",
    "scope",
    "fetched_objects"
  ],
  "title": "ExpandReport",
  "type": "object"
}
```

## `narrow`

### `NarrowReport`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The result of a narrow: the paths dropped and the scope that remains.",
  "properties": {
    "dropped": {
      "description": "The in-scope prefixes dropped from this checkout.",
      "items": {
        "type": "string"
      },
      "type": "array"
    },
    "scope": {
      "description": "This checkout's materialization scope after the narrow.",
      "items": {
        "type": "string"
      },
      "type": "array"
    }
  },
  "required": [
    "dropped",
    "scope"
  ],
  "title": "NarrowReport",
  "type": "object"
}
```

## `scope`

### `ScopeStatus`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The sparse-workspace scope of the current bay (§7.6). An empty prefix list means the full\ntree (an unscoped bay or the main tree).",
  "properties": {
    "bay": {
      "description": "The active bay's name (`null` in the main tree).",
      "type": [
        "string",
        "null"
      ]
    },
    "fetch_scope": {
      "description": "The warehouse's fetch-scope prefixes (empty = fully fetched; a sparse franchise records\nits fetched prefixes here).",
      "items": {
        "type": "string"
      },
      "type": "array"
    },
    "materialization_scope": {
      "description": "The bay's in-scope prefixes (empty = the full tree).",
      "items": {
        "type": "string"
      },
      "type": "array"
    },
    "scoped": {
      "description": "Whether this bay is scoped (has a non-full materialization scope).",
      "type": "boolean"
    }
  },
  "required": [
    "scoped",
    "materialization_scope",
    "fetch_scope"
  ],
  "title": "ScopeStatus",
  "type": "object"
}
```

## `scope-prune`

### `PruneReport`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The result of a scope-prune: what was pruned, the fetch scope that remains, and how much was\n(or would be) freed.",
  "properties": {
    "all_resumed": {
      "description": "Whether every requested path was already outside the fetch scope before this call — a\npure resume of an earlier, interrupted prune, rather than a path pruned for the first\ntime here.",
      "type": "boolean"
    },
    "dry_run": {
      "description": "Whether this was a dry run (nothing changed).",
      "type": "boolean"
    },
    "freed": {
      "description": "Loose objects actually freed (`0` on a dry run).",
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    },
    "pruned": {
      "description": "The fetched path(s) pruned (forgotten) from the warehouse fetch scope.",
      "items": {
        "type": "string"
      },
      "type": "array"
    },
    "retained_shared": {
      "description": "Candidates kept because they are shared (by content hash) with a scope that is still\nfetched, or with a meta pallet. Distinct from `still_packed`: this content stays by\ndesign, not pending a future repack.",
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    },
    "scope": {
      "description": "The warehouse fetch scope after the prune.",
      "items": {
        "type": "string"
      },
      "type": "array"
    },
    "still_packed": {
      "description": "Candidate objects present only inside a pack: a loose delete cannot reclaim them and a\nreachability repack keeps them (they are still reachable history), so a scope-aware\nrepack is future work. Reported so the count is never silently lost.",
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    },
    "would_free": {
      "description": "Loose objects a prune would free (equals `freed` after a real run).",
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    }
  },
  "required": [
    "dry_run",
    "pruned",
    "scope",
    "freed",
    "would_free",
    "still_packed",
    "retained_shared",
    "all_resumed"
  ],
  "title": "PruneReport",
  "type": "object"
}
```

## `store`

### `StoreReport`

```json
{
  "$defs": {
    "Maintenance": {
      "description": "The maintenance picture: whether auto-maintenance is on, the effective thresholds, and\nwhether either action is due now.",
      "properties": {
        "auto": {
          "description": "Whether background maintenance (`maintenance.auto`) is enabled.",
          "type": "boolean"
        },
        "compaction_due": {
          "description": "Whether an incremental compaction is due now.",
          "type": "boolean"
        },
        "loose_threshold": {
          "description": "Loose-object count above which an incremental compaction is due (`maintenance.loose`).",
          "format": "uint",
          "minimum": 0,
          "type": "integer"
        },
        "pack_threshold": {
          "description": "Pack-count above which a consolidating repack is due (`maintenance.packs`).",
          "format": "uint",
          "minimum": 0,
          "type": "integer"
        },
        "repack_due": {
          "description": "Whether a consolidating repack is due now.",
          "type": "boolean"
        }
      },
      "required": [
        "auto",
        "loose_threshold",
        "pack_threshold",
        "compaction_due",
        "repack_due"
      ],
      "type": "object"
    },
    "PackReport": {
      "description": "One pack's line in the census.",
      "properties": {
        "bytes": {
          "description": "On-disk bytes of the pack (data file + index file).",
          "format": "uint64",
          "minimum": 0,
          "type": "integer"
        },
        "deltas": {
          "description": "Of `objects`, how many are stored as deltas.",
          "format": "uint",
          "minimum": 0,
          "type": "integer"
        },
        "id": {
          "description": "The pack's id (its file stem).",
          "type": "string"
        },
        "objects": {
          "description": "Objects the pack holds.",
          "format": "uint",
          "minimum": 0,
          "type": "integer"
        }
      },
      "required": [
        "id",
        "objects",
        "deltas",
        "bytes"
      ],
      "type": "object"
    }
  },
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The object-store census. The `Serialize` shape is the public `--json` schema; byte counts\nare exact integers there (the human view renders them in binary units).",
  "properties": {
    "deltas": {
      "description": "Objects stored as deltas across all packs.",
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    },
    "densify_suggested": {
      "description": "Whether this store was bulk-ingested (`import-git`, or a franchise's native bundle\ninstall) and would still benefit from a one-shot `compact --all --redelta` pass.\nAdditive field; never acted on automatically.",
      "type": "boolean"
    },
    "loose_bytes": {
      "description": "Total on-disk bytes of the loose objects.",
      "format": "uint64",
      "minimum": 0,
      "type": "integer"
    },
    "loose_objects": {
      "description": "Loose (unpacked) object files.",
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    },
    "maintenance": {
      "$ref": "#/$defs/Maintenance",
      "description": "The maintenance thresholds and the current verdict."
    },
    "pack_bytes": {
      "description": "Total on-disk bytes of the packs.",
      "format": "uint64",
      "minimum": 0,
      "type": "integer"
    },
    "pack_files": {
      "description": "Number of pack files.",
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    },
    "packed_objects": {
      "description": "Objects held across all packs.",
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    },
    "packs": {
      "description": "One entry per pack file.",
      "items": {
        "$ref": "#/$defs/PackReport"
      },
      "type": "array"
    },
    "total_bytes": {
      "description": "Loose + packed bytes — the object store's on-disk footprint.",
      "format": "uint64",
      "minimum": 0,
      "type": "integer"
    }
  },
  "required": [
    "loose_objects",
    "loose_bytes",
    "packed_objects",
    "pack_files",
    "deltas",
    "pack_bytes",
    "total_bytes",
    "packs",
    "maintenance",
    "densify_suggested"
  ],
  "title": "StoreReport",
  "type": "object"
}
```

## `tag`

### `Created`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The result of creating a tag.",
  "properties": {
    "name": {
      "type": "string"
    },
    "parcel": {
      "description": "The tag parcel on the @tags meta pallet.",
      "type": "string"
    },
    "subject": {
      "type": "string"
    }
  },
  "required": [
    "name",
    "subject",
    "parcel"
  ],
  "title": "Created",
  "type": "object"
}
```

### `TagList`

```json
{
  "$defs": {
    "TagView": {
      "description": "One tag, with its tagger resolved to signed identity metadata.",
      "properties": {
        "message": {
          "description": "The tag message (may be empty).",
          "type": "string"
        },
        "name": {
          "type": "string"
        },
        "parcel": {
          "description": "The @tags parcel that introduced the tag.",
          "type": "string"
        },
        "subject": {
          "description": "The parcel the tag points at.",
          "type": "string"
        },
        "tagged_at": {
          "description": "The tag creation time as RFC 3339 (UTC).",
          "type": "string"
        },
        "tagger": {
          "description": "The tagger's pseudonymous operator id (the chain's record).",
          "type": "string"
        },
        "tagger_name": {
          "description": "The resolved display name, when a resolution hook supplied one.",
          "type": [
            "string",
            "null"
          ]
        },
        "tagger_role": {
          "description": "The tagger's role in the office, when known — so a reader can confirm the tag was\ncut by an admin (the release convention).",
          "type": [
            "string",
            "null"
          ]
        }
      },
      "required": [
        "name",
        "subject",
        "message",
        "tagger",
        "tagged_at",
        "parcel"
      ],
      "type": "object"
    }
  },
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The list of tags.",
  "properties": {
    "tags": {
      "items": {
        "$ref": "#/$defs/TagView"
      },
      "type": "array"
    }
  },
  "required": [
    "tags"
  ],
  "title": "TagList",
  "type": "object"
}
```

### `TagView`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "One tag, with its tagger resolved to signed identity metadata.",
  "properties": {
    "message": {
      "description": "The tag message (may be empty).",
      "type": "string"
    },
    "name": {
      "type": "string"
    },
    "parcel": {
      "description": "The @tags parcel that introduced the tag.",
      "type": "string"
    },
    "subject": {
      "description": "The parcel the tag points at.",
      "type": "string"
    },
    "tagged_at": {
      "description": "The tag creation time as RFC 3339 (UTC).",
      "type": "string"
    },
    "tagger": {
      "description": "The tagger's pseudonymous operator id (the chain's record).",
      "type": "string"
    },
    "tagger_name": {
      "description": "The resolved display name, when a resolution hook supplied one.",
      "type": [
        "string",
        "null"
      ]
    },
    "tagger_role": {
      "description": "The tagger's role in the office, when known — so a reader can confirm the tag was\ncut by an admin (the release convention).",
      "type": [
        "string",
        "null"
      ]
    }
  },
  "required": [
    "name",
    "subject",
    "message",
    "tagger",
    "tagged_at",
    "parcel"
  ],
  "title": "TagView",
  "type": "object"
}
```

## `undo`

### `Undone`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The result of an undo.",
  "properties": {
    "description": {
      "description": "The undone parcel's description (for orientation), when there is one.",
      "type": "string"
    },
    "head": {
      "description": "The pallet's head after the undo.",
      "type": "string"
    },
    "left": {
      "description": "For a reversed `shift`, the pallet left behind.",
      "type": "string"
    },
    "op": {
      "description": "The operation that was reversed (`stack`, `consolidate`, `shift`).",
      "type": "string"
    },
    "pallet": {
      "description": "The pallet that is current after the undo.",
      "type": "string"
    },
    "undone": {
      "description": "For a soft reset, the parcel that came off the pallet (its changes are staged again).",
      "type": "string"
    }
  },
  "required": [
    "op",
    "pallet",
    "left",
    "undone",
    "head",
    "description"
  ],
  "title": "Undone",
  "type": "object"
}
```

## `version`

### `Version`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The forklift version.",
  "properties": {
    "version": {
      "type": "string"
    }
  },
  "required": [
    "version"
  ],
  "title": "Version",
  "type": "object"
}
```


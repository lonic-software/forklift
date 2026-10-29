<!--
GENERATED FILE — do not edit by hand.
Produced by `bin/gen-docs` (crates/forklift/src/docgen.rs). CI and `bin/check` run
`bin/gen-docs --check`, which fails when this file is stale; run `bin/gen-docs` and commit
the result after a change that affects it (an error code, a command, alias or help line,
or a `--json` output struct).
-->

# `--json` output schemas

Every command's `--json` result is `{ "lonic_json", "tool", "schema", "command", "ok": true, "data": … }` on success (see `docs/MACHINE_INTERFACE.md` for the envelope and the failure shape). This page is the exhaustive reference for each command's `data` — one [JSON Schema](https://json-schema.org/) per shape a command can emit; a command with more than one (e.g. one per subcommand) lists all of them. Descriptions come straight from the Rust doc comments on the underlying struct, so they stay in sync with the field they describe. Each schema describes what Forklift writes: a field under `required` is always present (possibly as `null`), and any other field is left out when it has nothing to say.

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
      "description": "What a content audit checked, and (on a partial warehouse) what it sealed rather than verified.\nEmitted so a skimming reader or agent can never mistake a sparse pass — which verified content\nonly within the fetch scope — or a cache pass — which verified only the content it holds — for\na full-clone pass, nor a presence-only pass for a `--full` re-read.",
      "properties": {
        "cache": {
          "description": "The warehouse is a cache (franchised with `--cache`): it may lack content its history\nreaches, held by hash alone and counted in `held_by_hash`. Omitted on any other warehouse.",
          "type": "boolean"
        },
        "chunks": {
          "description": "How a chunked file's chunks were checked: `presence-checked` in a normal audit (bounded, no\nbytes re-read), or, under `--full`, re-read and re-hashed with each fully-present file\nre-assembled to verify its recipe's content hash.",
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
        "held_by_hash": {
          "description": "How many absent objects the warehouse's sealed record holds by hash alone: each is pinned to\nthe hash a parent that is present commits, but is not on disk, and nothing beneath it was\nchecked. Omitted when none.",
          "format": "uint",
          "minimum": 0,
          "type": "integer"
        },
        "in_scope_content": {
          "description": "In-scope content: every tree was re-hashed on read and every blob confirmed present, except\nthe absent objects counted in `held_by_hash`.",
          "type": "string"
        },
        "out_of_scope_content": {
          "description": "Out-of-scope content: sealed by the hash a signed parcel commits (unforgeable), verified\nwhen it is fetched. Present only on a sparse warehouse.",
          "type": [
            "string",
            "null"
          ]
        },
        "present_and_recorded": {
          "description": "How many objects the sealed record names that are on disk all the same. That fails\nnothing: a recorded object that is present is checked like any other. `heal` removes the\nrecords it can, and notes each one it keeps. Omitted when none.",
          "format": "uint",
          "minimum": 0,
          "type": "integer"
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
      "type": "boolean"
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
      "description": "The content audit that ran: present when the warehouse is partial, sparse or a cache (to\nreport what it may lack), or when `--full` re-verified content on a full clone. A normal full-clone audit is\nsignature-only and omits it, so a partial or presence-only pass is never mistaken for a\ncomplete content re-verification."
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
    "head"
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
    "pallet"
  ],
  "title": "CherryPicked",
  "type": "object"
}
```

## `compact`

### `Compacted`

```json
{
  "$defs": {
    "KeptDamagedPack": {
      "description": "A damaged pack a full repack kept.",
      "properties": {
        "error": {
          "description": "Why the index failed validation.",
          "type": "string"
        },
        "index_path": {
          "description": "The pack's index file.",
          "type": "string"
        },
        "unaccounted": {
          "description": "The live objects the pack might hold that no other copy serves verified: they cannot be\nrecovered from this store.",
          "items": {
            "type": "string"
          },
          "type": "array"
        }
      },
      "required": [
        "index_path",
        "error",
        "unaccounted"
      ],
      "type": "object"
    },
    "SignatureConflict": {
      "description": "A loose signature file a compaction left loose.",
      "properties": {
        "parcel": {
          "description": "The parcel the file signs.",
          "type": "string"
        },
        "reason": {
          "description": "Why it was left loose.",
          "type": "string"
        },
        "sidecar_path": {
          "description": "The file's path.",
          "type": "string"
        }
      },
      "required": [
        "parcel",
        "sidecar_path",
        "reason"
      ],
      "type": "object"
    }
  },
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
    "damaged_packs_dropped": {
      "description": "Damaged packs (an index that failed validation) a full repack salvaged and removed: every\nlive object they might have held reads verified from another copy. Each entry is the\nindex file's path. Always empty without `--all`.",
      "items": {
        "type": "string"
      },
      "type": "array"
    },
    "damaged_packs_kept": {
      "description": "Damaged packs a full repack salvaged what it could from but kept, because some live\nobject they might hold has no other verified copy. Always empty without `--all`.",
      "items": {
        "$ref": "#/$defs/KeptDamagedPack"
      },
      "type": "array"
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
    },
    "signature_conflicts": {
      "description": "Loose signature files left loose: one that differs from the signature already in a table\nfor its parcel (the tabled one is what reads return), or one that is malformed.",
      "items": {
        "$ref": "#/$defs/SignatureConflict"
      },
      "type": "array"
    },
    "signature_leftovers_removed": {
      "description": "Loose signature files removed because a table already held the same signature: leftover\ncopies, not signatures this run moved.",
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    },
    "signature_tables_unreadable": {
      "description": "Signature tables that could not be read, left as found. Each entry names the table and\nwhat to do about it.",
      "items": {
        "type": "string"
      },
      "type": "array"
    },
    "signature_tables_written": {
      "description": "Signature tables written: loose signatures moved into tables (a repack rewrites them all).",
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    },
    "signatures_dropped": {
      "description": "Signatures a full repack dropped because their parcel is no longer in the store. Always 0\nwithout `--all`.",
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    },
    "signatures_moved": {
      "description": "Loose signature files moved into a signature table this run wrote.",
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
    "over_ceiling_skipped",
    "damaged_packs_dropped",
    "damaged_packs_kept",
    "signature_tables_written",
    "signatures_moved",
    "signature_leftovers_removed",
    "signatures_dropped",
    "signature_conflicts",
    "signature_tables_unreadable"
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
      "description": "The value as written to the file, which is not always the argument given: a key with a\nfixed set of values (`remote.tor`) is stored in its documented spelling, so `\"  ON  \"` is\nreported, and stored, as `\"on\"`. A later `config <key>` read returns this same string.",
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
    "target"
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
      "description": "The git blobs written. A file unchanged from its parcel's first parent is reused, not\nwritten again.",
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
      "description": "The git trees written. A tree unchanged from its parcel's first parent is reused, not\nwritten again.",
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
  "$defs": {
    "BatchLaneClosure": {
      "description": "Why the remote's batch download stopped being used, after which the command fetched one\nobject at a time.",
      "oneOf": [
        {
          "const": "endpoint_absent",
          "description": "The remote does not serve batch downloads.",
          "type": "string"
        },
        {
          "const": "degraded",
          "description": "The remote was too slow to start a batch answer.",
          "type": "string"
        },
        {
          "const": "out_of_time",
          "description": "The remote ran short of time to answer a batch, and sent only part of it.",
          "type": "string"
        }
      ]
    }
  },
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The result of a franchise: what was imported and which pallet was checked out.",
  "properties": {
    "adopted_genesis": {
      "description": "The genesis of the remote's trust anchor, adopted on this first contact (absent when the\nremote has none).",
      "type": [
        "string",
        "null"
      ]
    },
    "batch_lane": {
      "anyOf": [
        {
          "$ref": "#/$defs/BatchLaneClosure"
        },
        {
          "type": "null"
        }
      ],
      "description": "Why the remote's batch download stopped being used, when it did. Absent when it stayed open."
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
    "pallet",
    "unborn",
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
          "description": "Approvals of the haul's current head; an approval of an earlier head is not counted.",
          "format": "uint",
          "minimum": 0,
          "type": "integer"
        },
        "id": {
          "type": "string"
        },
        "merge_proof": {
          "anyOf": [
            {
              "$ref": "#/$defs/MergeProofLine"
            },
            {
              "type": "null"
            }
          ],
          "description": "What this warehouse proves about the recorded merge; present exactly when merged."
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
    },
    "MergeProofLine": {
      "description": "What the local warehouse proves about a merged haul's recorded merge. A merge is recorded by\na writer of the target; this is judged on read, against the history held here.",
      "properties": {
        "reason": {
          "description": "Absent when verified. Refuted: `head_not_contained`. Unproven: `parcel_not_held`,\n`target_not_held`, `not_on_target` or `history_unreadable`.",
          "type": [
            "string",
            "null"
          ]
        },
        "verdict": {
          "description": "`verified` (the merge parcel contains the haul's head, and the target here contains the\nparcel), `refuted` (the merge parcel held here does not contain the head) or `unproven`.\nA reader meeting any other value treats it as not verified.",
          "type": "string"
        }
      },
      "required": [
        "verdict"
      ],
      "type": "object"
    },
    "UnreadableLine": {
      "description": "A haul, or a cell of one, that this forklift cannot read.",
      "properties": {
        "path": {
          "description": "The haul id, or the cell path within the pallet.",
          "type": "string"
        },
        "reason": {
          "type": "string"
        }
      },
      "required": [
        "path",
        "reason"
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
    },
    "unreadable": {
      "description": "Hauls and cells this forklift cannot read, whatever the state filter.",
      "items": {
        "$ref": "#/$defs/UnreadableLine"
      },
      "type": "array"
    }
  },
  "required": [
    "state",
    "hauls",
    "unreadable"
  ],
  "title": "HaulList",
  "type": "object"
}
```

### `HaulDetail`

```json
{
  "$defs": {
    "CommentLine": {
      "properties": {
        "author": {
          "type": "string"
        },
        "body": {
          "type": "string"
        },
        "commented_at": {
          "format": "int64",
          "type": "integer"
        }
      },
      "required": [
        "author",
        "body",
        "commented_at"
      ],
      "type": "object"
    },
    "MergeProofLine": {
      "description": "What the local warehouse proves about a merged haul's recorded merge. A merge is recorded by\na writer of the target; this is judged on read, against the history held here.",
      "properties": {
        "reason": {
          "description": "Absent when verified. Refuted: `head_not_contained`. Unproven: `parcel_not_held`,\n`target_not_held`, `not_on_target` or `history_unreadable`.",
          "type": [
            "string",
            "null"
          ]
        },
        "verdict": {
          "description": "`verified` (the merge parcel contains the haul's head, and the target here contains the\nparcel), `refuted` (the merge parcel held here does not contain the head) or `unproven`.\nA reader meeting any other value treats it as not verified.",
          "type": "string"
        }
      },
      "required": [
        "verdict"
      ],
      "type": "object"
    },
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
        "current": {
          "description": "Whether that is the haul's current head; a review of an earlier head no longer counts.",
          "type": "boolean"
        },
        "head": {
          "description": "The head the review is of.",
          "type": "string"
        },
        "reviewed_at": {
          "format": "int64",
          "type": "integer"
        },
        "verdict": {
          "type": "string"
        }
      },
      "required": [
        "author",
        "verdict",
        "body",
        "head",
        "current",
        "reviewed_at"
      ],
      "type": "object"
    },
    "UnreadableLine": {
      "description": "A haul, or a cell of one, that this forklift cannot read.",
      "properties": {
        "path": {
          "description": "The haul id, or the cell path within the pallet.",
          "type": "string"
        },
        "reason": {
          "type": "string"
        }
      },
      "required": [
        "path",
        "reason"
      ],
      "type": "object"
    }
  },
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "properties": {
    "comments": {
      "description": "Oldest first.",
      "items": {
        "$ref": "#/$defs/CommentLine"
      },
      "type": "array"
    },
    "description": {
      "type": "string"
    },
    "head": {
      "type": "string"
    },
    "id": {
      "type": "string"
    },
    "merge_parcel": {
      "description": "The merge parcel, when merged.",
      "type": [
        "string",
        "null"
      ]
    },
    "merge_proof": {
      "anyOf": [
        {
          "$ref": "#/$defs/MergeProofLine"
        },
        {
          "type": "null"
        }
      ],
      "description": "What this warehouse proves about the recorded merge; present exactly when merged."
    },
    "opened_at": {
      "format": "int64",
      "type": "integer"
    },
    "opener": {
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
    "title": {
      "type": "string"
    },
    "unreadable": {
      "description": "The haul's review and comment cells this forklift cannot read.",
      "items": {
        "$ref": "#/$defs/UnreadableLine"
      },
      "type": "array"
    }
  },
  "required": [
    "id",
    "title",
    "source",
    "target",
    "status",
    "head",
    "opener",
    "opened_at",
    "description",
    "reviews",
    "comments",
    "unreadable"
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

### `Updated`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "properties": {
    "changed": {
      "description": "False when the haul already proposed that head.",
      "type": "boolean"
    },
    "head": {
      "type": "string"
    },
    "id": {
      "type": "string"
    }
  },
  "required": [
    "id",
    "head",
    "changed"
  ],
  "title": "Updated",
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
  "$defs": {
    "OperationOutcome": {
      "description": "How `heal` settled an interrupted operation.",
      "oneOf": [
        {
          "const": "finished",
          "description": "It had already finished; only its record was left.",
          "type": "string"
        },
        {
          "const": "rolled_forward",
          "description": "It was completed.",
          "type": "string"
        },
        {
          "const": "rolled_back",
          "description": "It was undone: the bay is back where it started.",
          "type": "string"
        },
        {
          "const": "cleared",
          "description": "It had changed nothing that needed undoing; its record was cleared.",
          "type": "string"
        }
      ]
    },
    "OperationRecovery": {
      "description": "An interrupted operation `heal` settled.",
      "properties": {
        "base": {
          "description": "The parcel the bay stands on afterwards (absent for an unborn pallet).",
          "type": [
            "string",
            "null"
          ]
        },
        "operation": {
          "description": "The operation: shift, park, fast-forward, lower, bay creation, import-git, merge or\ncherry-pick.",
          "type": "string"
        },
        "outcome": {
          "$ref": "#/$defs/OperationOutcome"
        },
        "pallet": {
          "description": "The pallet the bay stands on afterwards.",
          "type": "string"
        },
        "unheld": {
          "description": "Tracked files the working directory does not hold at their recorded content. They read as\nlocal changes, never staged ones; `restore` brings them back.",
          "items": {
            "type": "string"
          },
          "type": "array"
        }
      },
      "required": [
        "operation",
        "outcome",
        "pallet"
      ],
      "type": "object"
    },
    "RecordRebuilt": {
      "description": "What `heal` dropped from a damaged sealed-object record.",
      "properties": {
        "at": {
          "description": "The byte the damage started at. Everything before it was kept.",
          "format": "uint",
          "minimum": 0,
          "type": "integer"
        },
        "dropped_bytes": {
          "description": "How many bytes from there on were dropped: the damage and any records after it.",
          "format": "uint",
          "minimum": 0,
          "type": "integer"
        }
      },
      "required": [
        "at",
        "dropped_bytes"
      ],
      "type": "object"
    }
  },
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The result of a `heal` run that fully resolved (or found nothing to resolve).",
  "properties": {
    "newly_held_by_hash": {
      "description": "Absent objects beneath those in `unsealed` that the record now holds by hash alone, so that\nremoving those records left nothing beneath them unaccounted for.",
      "items": {
        "type": "string"
      },
      "type": "array"
    },
    "notes": {
      "description": "Advisory notes that never blocked clearing: a note for each vanished inventory shard in\n`resolved`, a note for each bay whose local state could not be\nread this run (skipped rather than blocking recovery) naming the bay and how to clean it\nup, and — only when this run's own closure walk actually confirmed one — a note for each\ntrust pin (a trust or distrust boundary) that is not held locally. That last kind is never\nunconditional: a run with nothing left to check after restaging skips the closure walk\nentirely (there is nothing for it to resolve), so it reports no pin note even when one\ngenuinely applies; a run that still has something dangling reports no pin note either, since\nthis field only ever appears on a fully-cleared run. The sealed-object record adds a note\nfor each object on disk that keeps its record, and one when the record cannot be read.",
      "items": {
        "type": "string"
      },
      "type": "array"
    },
    "operation": {
      "anyOf": [
        {
          "$ref": "#/$defs/OperationRecovery"
        },
        {
          "type": "null"
        }
      ],
      "description": "The operation this run settled: one interrupted in this bay before it finished, which every\ncommand that changes the bay or commits staged content refused over until now."
    },
    "record_rebuilt": {
      "anyOf": [
        {
          "$ref": "#/$defs/RecordRebuilt"
        },
        {
          "type": "null"
        }
      ],
      "description": "What this run dropped from a damaged sealed-object record, which it rebuilt as the part\nbefore the damage. An object whose record was dropped reads as missing if it is absent."
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
    "unseal_failed": {
      "description": "Why removing those records failed, if it did. A failed write may have landed in part, so\n`unsealed` and `newly_held_by_hash` are then empty whatever the record holds.\nRunning `heal` again retries.",
      "type": [
        "string",
        "null"
      ]
    },
    "unsealed": {
      "description": "Objects on disk that the sealed-object record still named as held by hash alone (a refill\nwhose last write was lost leaves one), whose records this run removed.",
      "items": {
        "type": "string"
      },
      "type": "array"
    },
    "was_tainted": {
      "description": "Whether a durability taint was actually standing (an already-healthy warehouse reports\n`false`, with `restaged` and `resolved` empty).",
      "type": "boolean"
    }
  },
  "required": [
    "was_tainted",
    "restaged"
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
          "description": "Always \"recorded\": history reads the identity the parcel itself records (and joins\nthe office off it) without verifying the signature. `query --class` gives the verified\nreading; the tier is stamped here so no consumer can mistake one for the other.",
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
    "ExistingRule": {
      "description": "A rule of the ignore file, by where it stands.",
      "properties": {
        "line": {
          "description": "The line number, counting from 1.",
          "format": "uint",
          "minimum": 0,
          "type": "integer"
        },
        "pattern": {
          "description": "The pattern, as written on that line.",
          "type": "string"
        }
      },
      "required": [
        "line",
        "pattern"
      ],
      "type": "object"
    },
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
    "git_already_ignored": {
      "anyOf": [
        {
          "$ref": "#/$defs/ExistingRule"
        },
        {
          "type": "null"
        }
      ],
      "description": "The rule already in `.forkliftignore` that covers `.git`, when one did and so none was\nadded."
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
    "unheld": {
      "description": "Every checked-out file that does not hold the content imported for it (an uncommitted\nedit or deletion, or a file git converted as it checked it out), in tree order. Each reads\nas an unstaged change. The warning names only the first few. A tracked `.forkliftignore`\nthe import appended the `.git` rule to is not listed, although it reads as changed, even\nwhen it also held an edit of its own before the import.",
      "items": {
        "type": "string"
      },
      "type": "array"
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
    "ignored_git"
  ],
  "title": "ImportReport",
  "type": "object"
}
```

## `lift`

### `LiftReport`

```json
{
  "$defs": {
    "BatchLaneClosure": {
      "description": "Why the remote's batch download stopped being used, after which the command fetched one\nobject at a time.",
      "oneOf": [
        {
          "const": "endpoint_absent",
          "description": "The remote does not serve batch downloads.",
          "type": "string"
        },
        {
          "const": "degraded",
          "description": "The remote was too slow to start a batch answer.",
          "type": "string"
        },
        {
          "const": "out_of_time",
          "description": "The remote ran short of time to answer a batch, and sent only part of it.",
          "type": "string"
        }
      ]
    },
    "MetaPalletFailure": {
      "description": "A meta pallet that failed to sync, and why.",
      "properties": {
        "error": {
          "type": "string"
        },
        "pallet": {
          "description": "The pallet's wire ref, e.g. `@kv`.",
          "type": "string"
        }
      },
      "required": [
        "pallet",
        "error"
      ],
      "type": "object"
    },
    "MirrorStatus": {
      "description": "What the mirror did with one pallet.",
      "oneOf": [
        {
          "const": "pushed",
          "description": "The export was pushed to git.",
          "type": "string"
        },
        {
          "const": "up_to_date",
          "description": "Git's branch already held the export.",
          "type": "string"
        },
        {
          "const": "already_ahead",
          "description": "Git's branch was already past the export, at a later lift another warehouse mirrored.",
          "type": "string"
        },
        {
          "const": "skipped",
          "description": "The pallet cannot be mirrored (the remote lacks it, or git refuses its name as a branch).",
          "type": "string"
        },
        {
          "const": "busy",
          "description": "Another mirror run held the mirror.",
          "type": "string"
        },
        {
          "const": "raced",
          "description": "Git's branch kept moving while the push was retried.",
          "type": "string"
        },
        {
          "const": "diverged",
          "description": "Git's branch holds a commit that is not part of the export; nothing was pushed.",
          "type": "string"
        },
        {
          "const": "layout_skew",
          "description": "Git's branch was written in another export layout; nothing was pushed.",
          "type": "string"
        },
        {
          "const": "damaged",
          "description": "The mirror's own repository holds something this build did not write.",
          "type": "string"
        },
        {
          "const": "failed",
          "description": "A fetch, export or push failed.",
          "type": "string"
        }
      ]
    },
    "PalletMirror": {
      "description": "One pallet's mirror outcome.",
      "properties": {
        "commit": {
          "description": "The git commit the branch is at, when it is in step with the export.",
          "type": [
            "string",
            "null"
          ]
        },
        "exported": {
          "description": "The parcels this run wrote as new git commits.",
          "format": "uint",
          "minimum": 0,
          "type": "integer"
        },
        "message": {
          "description": "Why the pallet was skipped or not mirrored, with the step that clears it.",
          "type": [
            "string",
            "null"
          ]
        },
        "pallet": {
          "type": "string"
        },
        "status": {
          "$ref": "#/$defs/MirrorStatus"
        }
      },
      "required": [
        "pallet",
        "status",
        "exported"
      ],
      "type": "object"
    }
  },
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The result of a lift: the pallet's outcome, plus the office lift when trust\nrequired its keys to reach the remote first.",
  "properties": {
    "auto_merged": {
      "description": "How many times the lift auto-merged a diverged remote before it went through\n(optimistic lift).",
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    },
    "batch_lane": {
      "anyOf": [
        {
          "$ref": "#/$defs/BatchLaneClosure"
        },
        {
          "type": "null"
        }
      ],
      "description": "Why the remote's batch download stopped being used while this lift fetched (an auto-merge),\nwhen it did. Absent when it stayed open."
    },
    "head": {
      "type": "string"
    },
    "meta_failed": {
      "description": "The meta pallets that failed to lift. Never in a successful report: a failure makes the\ncommand fail, after the rest have gone through.",
      "items": {
        "$ref": "#/$defs/MetaPalletFailure"
      },
      "type": "array"
    },
    "meta_pallets": {
      "description": "The meta pallets (e.g. `@manifest`) lifted with new parcels.",
      "items": {
        "type": "string"
      },
      "type": "array"
    },
    "mirror": {
      "anyOf": [
        {
          "$ref": "#/$defs/PalletMirror"
        },
        {
          "type": "null"
        }
      ],
      "description": "What the git mirror did with the lifted pallet (`mirror.url`, `mirror.auto`). Absent when\nno mirror applies to it. A mirror failure is reported here and never fails the lift."
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
    "BatchLaneClosure": {
      "description": "Why the remote's batch download stopped being used, after which the command fetched one\nobject at a time.",
      "oneOf": [
        {
          "const": "endpoint_absent",
          "description": "The remote does not serve batch downloads.",
          "type": "string"
        },
        {
          "const": "degraded",
          "description": "The remote was too slow to start a batch answer.",
          "type": "string"
        },
        {
          "const": "out_of_time",
          "description": "The remote ran short of time to answer a batch, and sent only part of it.",
          "type": "string"
        }
      ]
    },
    "LowerOutcome": {
      "description": "What a lower did to the pallet it lowered.",
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
        },
        {
          "const": "created",
          "description": "The pallet did not exist here, and now points at the remote head. Only a named lower, of a\npallet other than the current one, creates one.",
          "type": "string"
        }
      ]
    },
    "MetaConflictEntry": {
      "description": "A path both sides changed differently since they last agreed.",
      "properties": {
        "ours": {
          "$ref": "#/$defs/MetaConflictSide"
        },
        "path": {
          "description": "The path as `--keep` spells it, e.g. `<subject>/<key>` on `@kv`.",
          "type": "string"
        },
        "theirs": {
          "$ref": "#/$defs/MetaConflictSide"
        }
      },
      "required": [
        "path",
        "ours",
        "theirs"
      ],
      "type": "object"
    },
    "MetaConflictSide": {
      "description": "One side's value at a conflicted path.",
      "properties": {
        "signer": {
          "description": "The key id the value is stamped with, when there is a readable value.",
          "type": [
            "string",
            "null"
          ]
        },
        "value": {
          "description": "The value, as the pallet shows it; absent when this side removed it.",
          "type": [
            "string",
            "null"
          ]
        }
      },
      "type": "object"
    },
    "MetaPalletConflicts": {
      "description": "A meta pallet that conflicted, with every conflicted path. Nothing was lifted for it.",
      "properties": {
        "conflicts": {
          "items": {
            "$ref": "#/$defs/MetaConflictEntry"
          },
          "type": "array"
        },
        "pallet": {
          "description": "The pallet's wire ref, e.g. `@kv`.",
          "type": "string"
        }
      },
      "required": [
        "pallet",
        "conflicts"
      ],
      "type": "object"
    },
    "MetaPalletFailure": {
      "description": "A meta pallet that failed to sync, and why.",
      "properties": {
        "error": {
          "type": "string"
        },
        "pallet": {
          "description": "The pallet's wire ref, e.g. `@kv`.",
          "type": "string"
        }
      },
      "required": [
        "pallet",
        "error"
      ],
      "type": "object"
    }
  },
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The result of a lower: the trust sync that ran first, then the pallet's outcome.",
  "properties": {
    "adopted_genesis": {
      "description": "The genesis of the remote's trust anchor, when it was adopted on this first contact.",
      "type": [
        "string",
        "null"
      ]
    },
    "batch_lane": {
      "anyOf": [
        {
          "$ref": "#/$defs/BatchLaneClosure"
        },
        {
          "type": "null"
        }
      ],
      "description": "Why the remote's batch download stopped being used, when it did. Absent when it stayed open."
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
    "meta_conflicts": {
      "description": "Meta pallets that conflicted. Never in a successful report: their paths are carried in\nthe `meta_conflict` error's details.",
      "items": {
        "$ref": "#/$defs/MetaPalletConflicts"
      },
      "type": "array"
    },
    "meta_failed": {
      "description": "Meta pallets that failed to sync. Never in a successful report.",
      "items": {
        "$ref": "#/$defs/MetaPalletFailure"
      },
      "type": "array"
    },
    "meta_replayed": {
      "description": "Meta pallets (e.g. `@kv`) whose local net change was replayed onto the remote head and\nlifted.",
      "items": {
        "type": "string"
      },
      "type": "array"
    },
    "meta_restamped": {
      "description": "Cells a replay re-signed under the current key, as `<pallet> <path>`: their stamp was\none of the operator's own older keys.",
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
      "anyOf": [
        {
          "$ref": "#/$defs/LowerOutcome"
        },
        {
          "type": "null"
        }
      ],
      "description": "What happened to the pallet lowered (`pallet`); always present in a successful report."
    },
    "pallet": {
      "type": "string"
    }
  },
  "required": [
    "office_moved",
    "pallet",
    "head",
    "fetched_objects",
    "fetched_signatures"
  ],
  "title": "LowerReport",
  "type": "object"
}
```

## `kv`

### `Changed`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "A key set or removed.",
  "properties": {
    "action": {
      "description": "`set` or `unset`.",
      "type": "string"
    },
    "key": {
      "type": "string"
    },
    "kv_head": {
      "description": "The head of the kv pallet after the change.",
      "type": [
        "string",
        "null"
      ]
    },
    "operator": {
      "type": "string"
    },
    "subject": {
      "type": "string"
    },
    "unchanged": {
      "description": "Whether the key already held exactly this value from this key, so nothing was written.",
      "type": "boolean"
    },
    "value": {
      "description": "The value set; absent for an unset.",
      "type": [
        "string",
        "null"
      ]
    }
  },
  "required": [
    "action",
    "subject",
    "key",
    "operator",
    "unchanged"
  ],
  "title": "Changed",
  "type": "object"
}
```

### `EntryView`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "One value, with who wrote it.",
  "properties": {
    "key": {
      "type": "string"
    },
    "signer": {
      "description": "The key id the value is stamped with.",
      "type": [
        "string",
        "null"
      ]
    },
    "subject": {
      "type": "string"
    },
    "unreadable": {
      "description": "Why the value cannot be read: a newer forklift's schema, or a malformed cell.",
      "type": [
        "string",
        "null"
      ]
    },
    "value": {
      "description": "The text; absent when this forklift cannot read the value (see `unreadable`).",
      "type": [
        "string",
        "null"
      ]
    }
  },
  "required": [
    "subject",
    "key"
  ],
  "title": "EntryView",
  "type": "object"
}
```

### `KvListing`

```json
{
  "$defs": {
    "EntryView": {
      "description": "One value, with who wrote it.",
      "properties": {
        "key": {
          "type": "string"
        },
        "signer": {
          "description": "The key id the value is stamped with.",
          "type": [
            "string",
            "null"
          ]
        },
        "subject": {
          "type": "string"
        },
        "unreadable": {
          "description": "Why the value cannot be read: a newer forklift's schema, or a malformed cell.",
          "type": [
            "string",
            "null"
          ]
        },
        "value": {
          "description": "The text; absent when this forklift cannot read the value (see `unreadable`).",
          "type": [
            "string",
            "null"
          ]
        }
      },
      "required": [
        "subject",
        "key"
      ],
      "type": "object"
    }
  },
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "Every value, or a parcel's.",
  "properties": {
    "entries": {
      "items": {
        "$ref": "#/$defs/EntryView"
      },
      "type": "array"
    },
    "subject": {
      "description": "The parcel the listing is scoped to (absent when listing every value).",
      "type": [
        "string",
        "null"
      ]
    }
  },
  "required": [
    "entries"
  ],
  "title": "KvListing",
  "type": "object"
}
```

### `KvLog`

```json
{
  "$defs": {
    "ChangeView": {
      "description": "One change to a key.",
      "properties": {
        "operator": {
          "description": "The operator the parcel records.",
          "type": "string"
        },
        "parcel": {
          "description": "The parcel that made the change.",
          "type": "string"
        },
        "removed": {
          "description": "Whether the change removed the value.",
          "type": "boolean"
        },
        "signer": {
          "description": "The key id that signed it.",
          "type": [
            "string",
            "null"
          ]
        },
        "timestamp": {
          "description": "When the parcel was stacked (Unix seconds).",
          "format": "int64",
          "type": "integer"
        },
        "unreadable": {
          "description": "Why the value after the change cannot be read.",
          "type": [
            "string",
            "null"
          ]
        },
        "value": {
          "description": "The value after the change; absent for a removal or an unreadable value.",
          "type": [
            "string",
            "null"
          ]
        }
      },
      "required": [
        "parcel",
        "operator",
        "timestamp",
        "removed"
      ],
      "type": "object"
    }
  },
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "Every change to a key, newest first.",
  "properties": {
    "changes": {
      "items": {
        "$ref": "#/$defs/ChangeView"
      },
      "type": "array"
    },
    "key": {
      "type": "string"
    },
    "subject": {
      "type": "string"
    }
  },
  "required": [
    "subject",
    "key",
    "changes"
  ],
  "title": "KvLog",
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
      "description": "The head of the manifest pallet that holds the entry.",
      "type": [
        "string",
        "null"
      ]
    },
    "operator": {
      "type": "string"
    },
    "subject": {
      "type": "string"
    },
    "unchanged": {
      "description": "Whether the operator had already recorded exactly this entry, so nothing was written.",
      "type": "boolean"
    }
  },
  "required": [
    "kind",
    "subject",
    "operator",
    "body",
    "unchanged",
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
          "description": "When a provenance entry was recorded (Unix seconds); only provenance carries a time.",
          "format": "int64",
          "type": [
            "integer",
            "null"
          ]
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
        "body"
      ],
      "type": "object"
    },
    "UnreadableView": {
      "description": "A manifest cell this forklift cannot read.",
      "properties": {
        "path": {
          "description": "The cell's path in the pallet.",
          "type": "string"
        },
        "reason": {
          "description": "Why it cannot be read.",
          "type": "string"
        },
        "subject": {
          "description": "The subject, when the cell's path names one.",
          "type": [
            "string",
            "null"
          ]
        }
      },
      "required": [
        "path",
        "reason"
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
    },
    "unreadable": {
      "description": "Entries this forklift cannot read — a newer kind or schema, or a malformed cell. Listed,\nnever dropped.",
      "items": {
        "$ref": "#/$defs/UnreadableView"
      },
      "type": "array"
    }
  },
  "required": [
    "entries"
  ],
  "title": "ManifestView",
  "type": "object"
}
```

## `mirror`

### `MirrorReport`

```json
{
  "$defs": {
    "MirrorStatus": {
      "description": "What the mirror did with one pallet.",
      "oneOf": [
        {
          "const": "pushed",
          "description": "The export was pushed to git.",
          "type": "string"
        },
        {
          "const": "up_to_date",
          "description": "Git's branch already held the export.",
          "type": "string"
        },
        {
          "const": "already_ahead",
          "description": "Git's branch was already past the export, at a later lift another warehouse mirrored.",
          "type": "string"
        },
        {
          "const": "skipped",
          "description": "The pallet cannot be mirrored (the remote lacks it, or git refuses its name as a branch).",
          "type": "string"
        },
        {
          "const": "busy",
          "description": "Another mirror run held the mirror.",
          "type": "string"
        },
        {
          "const": "raced",
          "description": "Git's branch kept moving while the push was retried.",
          "type": "string"
        },
        {
          "const": "diverged",
          "description": "Git's branch holds a commit that is not part of the export; nothing was pushed.",
          "type": "string"
        },
        {
          "const": "layout_skew",
          "description": "Git's branch was written in another export layout; nothing was pushed.",
          "type": "string"
        },
        {
          "const": "damaged",
          "description": "The mirror's own repository holds something this build did not write.",
          "type": "string"
        },
        {
          "const": "failed",
          "description": "A fetch, export or push failed.",
          "type": "string"
        }
      ]
    },
    "PalletMirror": {
      "description": "One pallet's mirror outcome.",
      "properties": {
        "commit": {
          "description": "The git commit the branch is at, when it is in step with the export.",
          "type": [
            "string",
            "null"
          ]
        },
        "exported": {
          "description": "The parcels this run wrote as new git commits.",
          "format": "uint",
          "minimum": 0,
          "type": "integer"
        },
        "message": {
          "description": "Why the pallet was skipped or not mirrored, with the step that clears it.",
          "type": [
            "string",
            "null"
          ]
        },
        "pallet": {
          "type": "string"
        },
        "status": {
          "$ref": "#/$defs/MirrorStatus"
        }
      },
      "required": [
        "pallet",
        "status",
        "exported"
      ],
      "type": "object"
    }
  },
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The result of a mirror run.",
  "properties": {
    "pallets": {
      "description": "Each mirrored pallet's outcome.",
      "items": {
        "$ref": "#/$defs/PalletMirror"
      },
      "type": "array"
    },
    "url": {
      "description": "The git remote mirrored to (`mirror.url`).",
      "type": "string"
    }
  },
  "required": [
    "url",
    "pallets"
  ],
  "title": "MirrorReport",
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
    "enrolled"
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
    "pallet",
    "head"
  ],
  "title": "Palletized",
  "type": "object"
}
```

### `Deleted`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "A deleted pallet.",
  "properties": {
    "contained_by": {
      "description": "What else holds that head (`pallet \"<name>\"` or `the remote`); `null` when the delete was\nforced without asking.",
      "type": [
        "string",
        "null"
      ]
    },
    "head": {
      "description": "The head it was deleted at; its parcels stay, and `undo` brings the pallet back here.",
      "type": "string"
    },
    "merged_hauls": {
      "description": "Merged hauls whose target this was: their proof now reads as unproven, since no pallet\nholds the target any more. Never a claim the record is false.",
      "items": {
        "type": "string"
      },
      "type": "array"
    },
    "pallet": {
      "description": "The pallet that was deleted.",
      "type": "string"
    }
  },
  "required": [
    "pallet",
    "head",
    "contained_by"
  ],
  "title": "Deleted",
  "type": "object"
}
```

### `Restored`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "A pallet brought back from its tombstone.",
  "properties": {
    "head": {
      "description": "The head it is back at: the one its tombstone held.",
      "type": "string"
    },
    "pallet": {
      "description": "The pallet that was brought back.",
      "type": "string"
    },
    "remote": {
      "description": "The remote's URL, when the pallet was restored there (`--remote`); omitted for this\nwarehouse's own.",
      "type": [
        "string",
        "null"
      ]
    }
  },
  "required": [
    "pallet",
    "head"
  ],
  "title": "Restored",
  "type": "object"
}
```

### `RemoteDeleted`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "A pallet deleted on the remote.",
  "properties": {
    "already": {
      "description": "Whether the remote had deleted it already (at `head`), as a rerun of a delete that landed\nfinds it: nothing was sent.",
      "type": "boolean"
    },
    "head": {
      "description": "The head it was deleted at, which the remote keeps under its tombstone.",
      "type": "string"
    },
    "local_head": {
      "description": "The local pallet's head, untouched by the delete; `null` when there is no local pallet of\nthat name.",
      "type": [
        "string",
        "null"
      ]
    },
    "pallet": {
      "description": "The pallet that was deleted there.",
      "type": "string"
    },
    "remote": {
      "description": "The remote's URL.",
      "type": "string"
    }
  },
  "required": [
    "remote",
    "pallet",
    "head",
    "local_head",
    "already"
  ],
  "title": "RemoteDeleted",
  "type": "object"
}
```

### `PalletList`

```json
{
  "$defs": {
    "DeletedEntry": {
      "description": "A deleted pallet whose tombstone still stands.",
      "properties": {
        "head": {
          "description": "The head it was deleted at, which the tombstone keeps.",
          "type": "string"
        },
        "name": {
          "type": "string"
        }
      },
      "required": [
        "name",
        "head"
      ],
      "type": "object"
    },
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
        "current",
        "head"
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
    "deleted": {
      "description": "The pallets deleted in this warehouse since a server served it, whose tombstones still\nstand; `palletize --restore <name>` brings one back. Omitted when empty.",
      "items": {
        "$ref": "#/$defs/DeletedEntry"
      },
      "type": "array"
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
    "pallets"
  ],
  "title": "PalletList",
  "type": "object"
}
```

### `RemotePalletList`

```json
{
  "$defs": {
    "RemoteDeletedEntry": {
      "description": "A pallet deleted on the remote.",
      "properties": {
        "head": {
          "description": "The head it was deleted at, which the remote keeps under its tombstone.",
          "type": "string"
        },
        "local_head": {
          "description": "The local pallet's head, which the remote's delete leaves alone; `null` when there is no\nlocal pallet of that name.",
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
        "head",
        "local_head"
      ],
      "type": "object"
    },
    "RemotePalletEntry": {
      "description": "One remote pallet.",
      "properties": {
        "local_head": {
          "description": "The local pallet's head parcel hash; `null` when there is no local pallet of that name.",
          "type": [
            "string",
            "null"
          ]
        },
        "name": {
          "type": "string"
        },
        "relation": {
          "$ref": "#/$defs/RemoteRelation"
        },
        "remote_head": {
          "description": "The remote's head parcel hash.",
          "type": "string"
        }
      },
      "required": [
        "name",
        "remote_head",
        "local_head",
        "relation"
      ],
      "type": "object"
    },
    "RemoteRelation": {
      "description": "How a local pallet relates to the remote's pallet of the same name.",
      "oneOf": [
        {
          "const": "absent",
          "description": "There is no local pallet of that name; `lower <name>` creates it.",
          "type": "string"
        },
        {
          "const": "same",
          "description": "The local pallet is at the remote head.",
          "type": "string"
        },
        {
          "const": "local_ahead",
          "description": "The local pallet holds the remote head and more; `lift` publishes it.",
          "type": "string"
        },
        {
          "const": "differs",
          "description": "The local pallet is behind the remote or diverged from it; `lower <name>` tells which,\nfast-forwarding a pallet that is behind.",
          "type": "string"
        }
      ]
    }
  },
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The remote's pallets, each against the local pallet of that name.",
  "properties": {
    "deleted": {
      "description": "The pallets deleted on the remote whose tombstones still stand; the remote keeps each one's\nhistory, and `palletize --restore <name> --remote` brings it back. Omitted when empty.",
      "items": {
        "$ref": "#/$defs/RemoteDeletedEntry"
      },
      "type": "array"
    },
    "meta": {
      "description": "The remote's meta pallets in their qualified form (`@office`), listed only when `--all`\nwas given; omitted when empty.",
      "items": {
        "$ref": "#/$defs/RemotePalletEntry"
      },
      "type": "array"
    },
    "pallets": {
      "description": "Every user pallet the remote has with something stacked.",
      "items": {
        "$ref": "#/$defs/RemotePalletEntry"
      },
      "type": "array"
    },
    "remote": {
      "description": "The remote's URL.",
      "type": "string"
    },
    "unnamed": {
      "description": "Pallet names the remote lists that this build cannot read (another version's naming\nrules), so they are not compared; omitted when empty.",
      "items": {
        "type": "string"
      },
      "type": "array"
    }
  },
  "required": [
    "remote",
    "pallets"
  ],
  "title": "RemotePalletList",
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
    "object_type"
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
        "actions"
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

## `self-schema`

### `SelfSchema`

```json
{
  "$defs": {
    "Flag": {
      "properties": {
        "arg": {
          "description": "The word passed when the switch is on.",
          "type": "string"
        }
      },
      "required": [
        "arg"
      ],
      "type": "object"
    },
    "Input": {
      "properties": {
        "secret": {
          "description": "The value is a credential.",
          "type": "boolean"
        }
      },
      "type": "object"
    },
    "Param": {
      "description": "One param: its kind, and what its value is for.",
      "oneOf": [
        {
          "description": "A required single positional; Pult prompts for it when it is absent.",
          "properties": {
            "input": {
              "$ref": "#/$defs/Input"
            }
          },
          "required": [
            "input"
          ],
          "type": "object"
        },
        {
          "description": "A switch.",
          "properties": {
            "flag": {
              "$ref": "#/$defs/Flag"
            }
          },
          "required": [
            "flag"
          ],
          "type": "object"
        },
        {
          "description": "An option (with `arg`) or the trailing positionals (without).",
          "properties": {
            "repeat": {
              "$ref": "#/$defs/Repeat"
            }
          },
          "required": [
            "repeat"
          ],
          "type": "object"
        }
      ],
      "properties": {
        "description": {
          "description": "What the value is for, from the argument's help; absent when it has none.",
          "type": [
            "string",
            "null"
          ]
        }
      },
      "type": "object",
      "unevaluatedProperties": false
    },
    "Repeat": {
      "properties": {
        "arg": {
          "description": "The option each value follows; absent for positionals.",
          "type": [
            "string",
            "null"
          ]
        },
        "max": {
          "description": "The most values accepted; unbounded when absent.",
          "format": "uint32",
          "minimum": 0,
          "type": [
            "integer",
            "null"
          ]
        },
        "min": {
          "description": "The fewest values accepted.",
          "format": "uint32",
          "minimum": 0,
          "type": "integer"
        },
        "secret": {
          "description": "Every value is a credential.",
          "type": "boolean"
        }
      },
      "type": "object"
    },
    "SchemaCommand": {
      "description": "One runnable command path.",
      "properties": {
        "category": {
          "description": "The top-level group the command belongs to; absent for a top-level command.",
          "type": [
            "string",
            "null"
          ]
        },
        "description": {
          "description": "The command's long description, when it has one.",
          "type": [
            "string",
            "null"
          ]
        },
        "id": {
          "description": "The command path joined with `-` (`office-admit`).",
          "type": "string"
        },
        "interactive": {
          "description": "Always false: no Forklift command requires a terminal by itself.",
          "type": "boolean"
        },
        "params": {
          "additionalProperties": {
            "$ref": "#/$defs/Param"
          },
          "description": "The command's params by key, in order: options and switches first, then positionals.",
          "type": "object"
        },
        "run": {
          "description": "The argument tail after the program name, with a `{key}` placeholder per param.\nOptions and switches come before a literal `--`, positionals after it.",
          "type": "string"
        },
        "title": {
          "description": "The command's one-line description.",
          "type": "string"
        }
      },
      "required": [
        "id",
        "title",
        "params",
        "run",
        "interactive"
      ],
      "type": "object"
    }
  },
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The command surface as a Pult manifest: exactly the top-level keys a root manifest may\ncarry, since Pult refuses unknown ones.",
  "properties": {
    "commands": {
      "description": "One command per runnable command path, in clap's declaration order.",
      "items": {
        "$ref": "#/$defs/SchemaCommand"
      },
      "type": "array"
    },
    "description": {
      "description": "The tool's one-line description.",
      "type": "string"
    },
    "name": {
      "description": "The tool's name (`forklift`).",
      "type": "string"
    },
    "version": {
      "description": "Pult's manifest version (1).",
      "format": "uint32",
      "minimum": 0,
      "type": "integer"
    }
  },
  "required": [
    "version",
    "name",
    "description",
    "commands"
  ],
  "title": "SelfSchema",
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

### `CaughtUp`

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "How a bay caught up with its pallet (see [`catch_up`]).",
  "properties": {
    "from": {
      "description": "The parcel the bay's files stood on before (empty for an unborn pallet).",
      "type": "string"
    },
    "head": {
      "description": "The pallet's head, which the bay now stands on (empty for an unborn pallet).",
      "type": "string"
    },
    "moved_files": {
      "description": "Whether the working directory and inventory were moved to the head's tree. False when the\nbay already stood on the head, or its files already matched the head's tree.",
      "type": "boolean"
    },
    "pallet": {
      "type": "string"
    }
  },
  "required": [
    "pallet",
    "from",
    "head",
    "moved_files"
  ],
  "title": "CaughtUp",
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
      "description": "The parcel this bay's files stand on, which staged changes are measured against: the\npallet's head unless the pallet moved since; `null` when it is unborn.",
      "type": [
        "string",
        "null"
      ]
    },
    "moved_to": {
      "description": "The pallet's head, when another bay or process moved the pallet past `head` since this\nbay's files were materialized. `shift <pallet>` catches up.",
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
      "description": "How many changes are staged (inventory vs `head`).",
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
    "head",
    "staged_count",
    "unstaged_count",
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
  "$defs": {
    "BatchLaneClosure": {
      "description": "Why the remote's batch download stopped being used, after which the command fetched one\nobject at a time.",
      "oneOf": [
        {
          "const": "endpoint_absent",
          "description": "The remote does not serve batch downloads.",
          "type": "string"
        },
        {
          "const": "degraded",
          "description": "The remote was too slow to start a batch answer.",
          "type": "string"
        },
        {
          "const": "out_of_time",
          "description": "The remote ran short of time to answer a batch, and sent only part of it.",
          "type": "string"
        }
      ]
    }
  },
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
    "batch_lane": {
      "anyOf": [
        {
          "$ref": "#/$defs/BatchLaneClosure"
        },
        {
          "type": "null"
        }
      ],
      "description": "Why the remote's batch download stopped being used, when it did. Absent when it stayed open."
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
  "description": "The sparse-workspace scope of the current bay. An empty prefix list means the full\ntree (an unscoped bay or the main tree).",
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
    "implicit_fetch_scope": {
      "description": "The paths a sparse fetch scope holds without listing them as prefixes: the root ignore\nfile, which the store fetches along with the prefixes. Empty when fully fetched.",
      "items": {
        "type": "string"
      },
      "type": "array"
    },
    "implicit_scope": {
      "description": "The paths a scoped bay holds in scope without listing them as prefixes: the root ignore\nfile, so its rules apply to the checkout. Empty when the bay is not scoped.",
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
    "implicit_scope",
    "fetch_scope",
    "implicit_fetch_scope"
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
    "kept_for_deleted": {
      "description": "Candidates kept because lifting the unlifted history of a pallet deleted here reads them,\nwhile `undo` can still bring that pallet back. Lifting cannot free these; a later prune\ndoes once the delete can no longer be undone.",
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    },
    "kept_for_hauls": {
      "description": "Candidates kept because lifting the unlifted history a haul not yet merged proposes reads\nthem, where no pallet reaches it any more. Lifting cannot free these; once the haul is\nmerged and its target lifted, a later prune does. A closed haul keeps them, since\nreopening it proposes the same head.",
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    },
    "kept_for_mirror": {
      "description": "Candidates kept because the git mirror's roots hold history the remote is not known to\nhold, where no pallet or haul reaches it. Normally zero, since the mirror exports only what\nthe remote held; a later prune frees them once the remote holds that history again.",
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    },
    "kept_unlifted": {
      "description": "Candidates kept because lifting history the remote does not hold reads them: work stacked\nhere and never lifted, or lifted to a remote head the remote did not confirm. Some may be the\nonly copy, the rest are base trees the lift compares against; a later prune of the same\npath frees them once that history is lifted.",
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
    "unlifted_pallets": {
      "description": "The pallets whose unlifted history changed the pruned path; lifting them (or lowering) is\nwhat lets a later prune free the rest. Pallets with unlifted work elsewhere are not listed,\nand it can be empty when gaps in the history hide which pallet leads to the kept work.",
      "items": {
        "type": "string"
      },
      "type": "array"
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
    "kept_unlifted",
    "kept_for_hauls",
    "kept_for_mirror",
    "kept_for_deleted",
    "unlifted_pallets",
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
        },
        "signature_table_threshold": {
          "description": "Count of signature tables below the size bound above which an incremental compaction is\ndue, to merge them (`maintenance.signatureTables`).",
          "format": "uint",
          "minimum": 0,
          "type": "integer"
        }
      },
      "required": [
        "auto",
        "loose_threshold",
        "pack_threshold",
        "signature_table_threshold",
        "compaction_due",
        "repack_due"
      ],
      "type": "object"
    },
    "PackIndexIssue": {
      "description": "One entry in any of [`StoreReport`]'s pack problem lists.",
      "properties": {
        "error": {
          "description": "Why it could not be fully trusted.",
          "type": "string"
        },
        "index_path": {
          "description": "The index file's path.",
          "type": "string"
        }
      },
      "required": [
        "index_path",
        "error"
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
    },
    "SignatureConflictIssue": {
      "description": "One entry in [`StoreReport::signature_conflicts`].",
      "properties": {
        "parcel": {
          "description": "The parcel the file signs.",
          "type": "string"
        },
        "reason": {
          "description": "Why a compaction leaves it loose.",
          "type": "string"
        },
        "sidecar_path": {
          "description": "The file's path.",
          "type": "string"
        }
      },
      "required": [
        "parcel",
        "sidecar_path",
        "reason"
      ],
      "type": "object"
    }
  },
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "description": "The object-store census. The `Serialize` shape is the public `--json` schema; byte counts\nare exact integers there (the human view renders them in binary units).",
  "properties": {
    "damaged_packs": {
      "description": "Packs that load but whose index is damaged in places: their sound parts are still read,\nand an object in a damaged part reads as unverifiable. `forklift compact --all` salvages\nwhat the pack's data file still holds, and removes the pack when every live object it may\nhold reads from another copy. Empty on a healthy store.",
      "items": {
        "$ref": "#/$defs/PackIndexIssue"
      },
      "type": "array"
    },
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
    "foreign_version_indexes": {
      "description": "Pack indexes written by a build with another pack format version: every read one might\nanswer fails naming the version. Re-create the warehouse with this build; moving one aside\nwould make its objects read as missing. Empty on a healthy store.",
      "items": {
        "$ref": "#/$defs/PackIndexIssue"
      },
      "type": "array"
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
    "loose_signatures": {
      "description": "Loose signature files a compaction would move into a table.",
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
    "partial_signature_tables": {
      "description": "Of `signature_tables`, how many are below the size bound, so a compaction would merge them.",
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    },
    "quarantined_packs": {
      "description": "Pack indexes that parsed but whose data file could not be loaded — their\nobjects are quarantined, not lost: `forklift heal` can still refetch any of them under a\ndurability taint, and restoring the data file recovers the rest. Empty on a healthy store.",
      "items": {
        "$ref": "#/$defs/PackIndexIssue"
      },
      "type": "array"
    },
    "signature_conflicts": {
      "description": "Loose signature files a compaction leaves loose: each differs from the signature already in\na table for its parcel (the tabled one is what reads return), or is malformed. Empty on a\nhealthy store.",
      "items": {
        "$ref": "#/$defs/SignatureConflictIssue"
      },
      "type": "array"
    },
    "signature_tables": {
      "description": "Signature tables: parcel signatures packed many to a file.",
      "format": "uint",
      "minimum": 0,
      "type": "integer"
    },
    "total_bytes": {
      "description": "Loose + packed bytes — the object store's on-disk footprint.",
      "format": "uint64",
      "minimum": 0,
      "type": "integer"
    },
    "unenumerable_indexes": {
      "description": "Pack index files that could not even be parsed — move the file aside and\nre-run. Empty on a healthy store.",
      "items": {
        "$ref": "#/$defs/PackIndexIssue"
      },
      "type": "array"
    },
    "unreadable_signature_tables": {
      "description": "Signature tables that cannot be read: every signature lookup one might answer fails naming\nit. `error` says what to do, which depends on the cause. Empty on a healthy store.",
      "items": {
        "$ref": "#/$defs/PackIndexIssue"
      },
      "type": "array"
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
    "densify_suggested",
    "quarantined_packs",
    "damaged_packs",
    "unenumerable_indexes",
    "foreign_version_indexes",
    "signature_tables",
    "partial_signature_tables",
    "loose_signatures",
    "unreadable_signature_tables",
    "signature_conflicts"
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
    "subject": {
      "type": "string"
    },
    "tags_head": {
      "description": "The new head of the @tags meta pallet.",
      "type": "string"
    }
  },
  "required": [
    "name",
    "subject",
    "tags_head"
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
        "signer": {
          "description": "The key id the tag is signed with.",
          "type": "string"
        },
        "subject": {
          "description": "The parcel the tag points at.",
          "type": "string"
        },
        "subject_absent": {
          "description": "Present and `true` when the subject parcel is not in this store (never fetched, or\ncollected); omitted when the subject is present.",
          "type": "boolean"
        },
        "tagged_at": {
          "description": "The tag creation time as RFC 3339 (UTC).",
          "type": "string"
        },
        "tagger": {
          "description": "The tagger's pseudonymous operator id (the chain's record), or the key id when the\noffice does not know the key.",
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
          "description": "The tagger's role in the office, when known.",
          "type": [
            "string",
            "null"
          ]
        }
      },
      "required": [
        "name",
        "subject",
        "tagger",
        "signer",
        "tagged_at"
      ],
      "type": "object"
    },
    "UnreadableView": {
      "description": "A tag this forklift cannot read.",
      "properties": {
        "name": {
          "type": "string"
        },
        "reason": {
          "description": "Why it cannot be read.",
          "type": "string"
        }
      },
      "required": [
        "name",
        "reason"
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
    },
    "unreadable": {
      "description": "Tags this forklift cannot read — a newer schema, or a malformed cell. Listed, never\ndropped.",
      "items": {
        "$ref": "#/$defs/UnreadableView"
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
    "signer": {
      "description": "The key id the tag is signed with.",
      "type": "string"
    },
    "subject": {
      "description": "The parcel the tag points at.",
      "type": "string"
    },
    "subject_absent": {
      "description": "Present and `true` when the subject parcel is not in this store (never fetched, or\ncollected); omitted when the subject is present.",
      "type": "boolean"
    },
    "tagged_at": {
      "description": "The tag creation time as RFC 3339 (UTC).",
      "type": "string"
    },
    "tagger": {
      "description": "The tagger's pseudonymous operator id (the chain's record), or the key id when the\noffice does not know the key.",
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
      "description": "The tagger's role in the office, when known.",
      "type": [
        "string",
        "null"
      ]
    }
  },
  "required": [
    "name",
    "subject",
    "tagger",
    "signer",
    "tagged_at"
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
      "description": "The operation that was reversed (`stack`, `consolidate`, `cherry-pick`, `shift`, `delete`).",
      "type": "string"
    },
    "pallet": {
      "description": "The pallet that is current after the undo.",
      "type": "string"
    },
    "restored": {
      "description": "For a reversed `delete`, the pallet that was deleted.",
      "type": "string"
    },
    "restored_as": {
      "description": "For a reversed `delete`, the name the pallet is back under: its own, or the one `--as` gave.",
      "type": "string"
    },
    "restored_head": {
      "description": "For a reversed `delete`, the head the pallet is back at (the one it was deleted at).",
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
    "head"
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


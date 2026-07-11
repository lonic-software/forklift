use clap::{Parser, Subcommand};

/// The forklift CLI: every command, alias, flag and help text lives here — this is the
/// single source of truth for the command surface. Handlers in `commands/` receive the
/// already-parsed values and own the presentation; all logic lives in `forklift-core`.
#[derive(Parser)]
#[command(
    name = "forklift",
    version,
    about = "Forklift — a version control system designed for serverless hosting.",
    arg_required_else_help = true,
    disable_help_subcommand = true,
    subcommand_value_name = "COMMAND",
    subcommand_help_heading = "Commands"
)]
pub struct Cli {
    /// Print more detail (e.g. unchanged diff lines, full inventory records).
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Emit a single JSON document instead of prose (for scripts and agents).
    #[arg(
        long,
        global = true,
        long_help = "Emit the command's result as one JSON document on stdout instead of prose. \
                     The envelope is \
                     { \"forklift_json\": <schema>, \"command\", \"ok\", \"data\" } on success and \
                     { \"forklift_json\", \"ok\": false, \"error\": { \"code\", \"message\", \
                     \"next_step\" } } on failure. Errors also set a deterministic exit code."
    )]
    pub json: bool,

    /// Do not pipe output through a pager (print straight to the terminal).
    #[arg(
        long,
        global = true,
        long_help = "Never pipe output through a pager. By default, when stdout is a terminal, \
                     long human output is shown in a pager (like git) — $FORKLIFT_PAGER, then \
                     $PAGER, then `less`. --no-pager, --json, and non-terminal output all print \
                     straight through."
    )]
    pub no_pager: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {

    /// Manage the `fl` short alias for this binary
    #[command(
        long_about = "Manage the `fl` short alias: a shell-agnostic symlink \
                      (a shim on Windows) placed next to this binary, not a per-shell alias — it \
                      works in scripts, non-interactive shells and every shell dialect alike, and \
                      is trivially discoverable and removable. This is the one implementation \
                      every install method (the curl script, pult, `cargo install`) calls, so the \
                      alias never drifts between install paths. Without a subcommand, reports \
                      whether the alias is installed and where."
    )]
    Alias {
        #[command(subcommand)]
        action: Option<AliasAction>,
    },

    /// Verify the warehouse's signed history offline
    #[command(
        visible_alias = "a",
        long_about = "Verify the warehouse's signed history offline: the office chain back to the \
                      genesis parcel, then the given pallet (the current one by default). Requires \
                      established trust (\"forklift office enroll\")."
    )]
    Audit {
        /// The pallet to audit (default: the current pallet)
        pallet: Option<String>,
    },

    /// Manage bays: parallel working directories bound to this warehouse
    #[command(
        long_about = "Manage bays: additional working directories bound to this warehouse. \
                      A bay shares the object store, the refs and trust, but keeps its own working \
                      tree, inventory, current pallet and lock — so several agents (or you and an \
                      agent) work one machine without cloning the objects N times or fighting one \
                      lock. Like git worktrees, designed in. Without a subcommand, the bays are \
                      listed."
    )]
    Bay {
        #[command(subcommand)]
        action: Option<BayAction>,
    },

    /// Show, for each line of a file, the parcel and author that last changed it
    #[command(
        visible_alias = "bl",
        alias = "annotate",
        long_about = "Attribute every line of a file to the parcel that introduced it — and, \
                      because authorship is signed and classed, to the author's identity \
                      class and supervisor. That is blame git cannot express: \"was this line \
                      written by a human or an agent, under whose supervision\", offline and \
                      forge-proof. The walk follows the first-parent chain from the revision (the \
                      current pallet's head by default); a line a merge brought in from a side \
                      line is attributed to the merge parcel."
    )]
    Blame {
        /// The file to blame
        path: String,

        /// The revision to blame at: a pallet name or a parcel hash (default: the current
        /// pallet's head)
        #[arg(short, long)]
        rev: Option<String>,
    },

    /// Apply a parcel's changes onto the current pallet as a new, author-preserving parcel
    #[command(
        name = "cherry-pick",
        visible_alias = "cp",
        long_about = "Apply a parcel's diff (its change against its first parent) onto the current \
                      pallet as a new, author-preserving, freshly-signed parcel. Unlike \
                      rebase, cherry-pick only *adds* — no rewrite, no force-push: the picked \
                      parcel's authors are preserved and this operator is recorded as the stacker. \
                      A clean pick is stacked immediately; a conflicting one leaves markers to \
                      resolve, \"load\" and \"stack\" (which completes the pick single-parent)."
    )]
    CherryPick {
        /// The parcel to cherry-pick: a pallet name, or a parcel hash / unique prefix
        revision: String,

        /// The new parcel's message (default: the source parcel's message)
        #[arg(short = 'm', long)]
        message: Option<String>,
    },

    /// Compact the object store: pack loose objects into a few dense pack files
    #[command(
        long_about = "Compact the object store: sweep the loose objects (each its own \
                      zstd-compressed file) into a few dense pack files — an append-only data \
                      file plus a sorted index. At scale that turns hundreds of thousands of \
                      tiny files into a handful of large ones, so a history walk stops paying an \
                      open-per-object and the store stops paying per-file slack. Objects are only \
                      moved, never re-hashed; a loose file is deleted only after the pack that \
                      holds it is durably written, so compacting can never lose an object. Packs \
                      roll over at a size/count threshold, so no single pack grows unbounded. \
                      Run it after a large \"import-git\" or periodically as maintenance. With \
                      --all, also rewrite the existing packs: drop unreachable (garbage) \
                      objects that were stuck in packs and consolidate many packs into few — a \
                      heavier full repack, worth running occasionally."
    )]
    Compact {
        /// Repack existing packs too: drop unreachable objects and consolidate (a full repack)
        #[arg(long)]
        all: bool,
    },

    /// Read or change configuration values
    #[command(
        visible_alias = "cfg",
        long_about = "Read or change configuration values (e.g. \"operator.name\"). Without a key, \
                      every known key is listed; with a key, its value is printed; with a key and a \
                      value, the value is set; with --unset, the key is removed (e.g. clearing a \
                      \"remote.token\"). Values are stored in the current warehouse by default; \
                      when reading, the warehouse value wins over the global one."
    )]
    Config {
        /// Operate on the per-user configuration that applies to all warehouses
        #[arg(short, long)]
        global: bool,

        /// Remove the key from the configuration (e.g. "config --unset remote.token")
        #[arg(long, conflicts_with = "value")]
        unset: bool,

        /// The configuration key (e.g. "operator.name"); omit it to list every key
        key: Option<String>,

        /// The value to set the key to; omit it to print the current value
        value: Option<String>,
    },

    /// List the files left in conflict by an unresolved consolidation
    #[command(
        long_about = "List the files an unresolved consolidation left in conflict, as structured \
                      records (with --json: each file's base, ours and theirs sides as content \
                      addresses a resolver can fetch). Designed for agents, which resolve merges \
                      well when given the three sides instead of marker soup. An empty list is a \
                      valid answer — there is nothing to resolve."
    )]
    Conflicts,

    /// Consolidate (merge) another pallet into the current one
    #[command(
        visible_alias = "con",
        alias = "merge",
        long_about = "Consolidate (merge) another pallet into the current one. Fast-forwards when \
                      possible; otherwise runs a three-way merge and stacks a merge parcel. \
                      Conflicts are marked in the affected files and must be resolved, loaded and \
                      stacked to complete the consolidation."
    )]
    Consolidate {
        /// The pallet whose head to merge into the current pallet
        pallet: String,
    },

    /// Squash the current draft pallet onto a target as one clean parcel, keeping the trail (`dv`)
    #[command(
        visible_alias = "dv",
        long_about = "Deliver the current (draft) pallet's checkpoint trail onto a target pallet \
                      as a single clean signed parcel — the squash agents need, without \
                      losing the trail. The delivered parcel carries the draft head's tree with \
                      the target as its only parent, so the checkpoints stay out of the target's \
                      history; the full trail is kept (the draft pallet is left intact) and \
                      recorded as a signed \"delivery\" entry on the parcel's manifest, so \"what \
                      the agent tried, in what order\" stays discoverable. Needs an enrolled key \
                      (the trail is signed evidence). The current pallet becomes the target; the \
                      working directory is unchanged (the tree is identical)."
    )]
    Deliver {
        /// The pallet to deliver onto
        target: String,

        /// The delivered parcel's message
        #[arg(short = 'm', long)]
        message: Option<String>,
    },

    /// Show line-by-line changes
    #[command(
        visible_alias = "d",
        long_about = "Show the line-by-line changes between the working directory and the \
                      inventory (what \"load\" would stage). With --staged, show the changes \
                      between the inventory and the pallet head (what \"stack\" would record). \
                      With two revisions (pallet names or parcel hashes), compare those instead. \
                      An optional trailing path limits the report to a file or directory."
    )]
    Diff {
        /// Compare the inventory against the pallet head (what "stack" would record)
        #[arg(short, long)]
        staged: bool,

        /// [path], or <revision> <revision> [path]
        #[arg(value_name = "REVISION|PATH")]
        targets: Vec<String>,
    },

    /// Export this warehouse's history into a new git repository (one-way, lossy)
    #[command(
        long_about = "One-way export of this warehouse's history into a new git repository: \
                      parcels become commits, trees become trees, blobs become blobs, and each user \
                      pallet becomes a branch. The escape hatch that makes trying forklift reversible \
                      — but lossy in this direction: git has no home for the signed office, the \
                      @manifest, or per-parcel signatures, so those are dropped. The path must be \
                      empty or new. Requires git on PATH."
    )]
    ExportGit {
        /// Where to create the git repository (must be empty or new)
        path: String,
    },

    /// Open a local franchise of a remote warehouse (clone)
    #[command(
        visible_alias = "fr",
        alias = "clone",
        long_about = "Open a local franchise of a remote warehouse: prepare a fresh warehouse in \
                      the target directory, remember the remote, adopt its trust anchor, download \
                      the history (using the remote's bundle when it has one) and materialize the \
                      chosen pallet in the working directory. With --only, franchise sparsely: \
                      fetch the full signed history but only the named subtree(s) of content, \
                      leaving the rest sealed by hash."
    )]
    Franchise {
        /// The URL of the remote warehouse (e.g. http://forklift.example.com:9418)
        url: String,

        /// The directory to create the warehouse in (must be new or empty)
        directory: String,

        /// The pallet to check out (default: the remote's default pallet)
        #[arg(short, long)]
        pallet: Option<String>,

        /// The bearer token, when the remote requires one (remembered as "remote.token")
        #[arg(short, long)]
        token: Option<String>,

        /// Franchise sparsely: fetch only the named subtree path(s) of content. Repeatable.
        #[arg(
            long = "only",
            value_name = "PATH",
            long_help = "Franchise sparsely: fetch the whole signed history — every parcel, \
                         signature and the tree spine — but only the content under the named \
                         subtree path(s). Out-of-scope subtrees and blobs are never downloaded; \
                         they stay pinned by the exact hash a signed parcel already commits, so \
                         nothing can be forged, it is simply not fetched. The working tree \
                         materializes only the in-scope subtree(s). The remote's whole-store \
                         bundle is skipped (it would defeat the point). Repeatable to fetch \
                         several subtrees. Widen later with \"expand\"."
        )]
        only: Vec<String>,
    },

    /// Reviewable merge proposals (pull requests) on the @haul meta pallet
    #[command(
        long_about = "Propose merging one pallet into another, with discussion and signed reviews — \
                      pull requests, in Forklift's vocabulary. A haul is an append-only log of signed events on the \
                      @haul meta pallet, so authorship — and who approved — is forge-proof and \
                      carries the operator's identity class (a human's approval is distinguishable \
                      from an agent's). Reviews are recorded; merging is not gated on them in this \
                      release. Intra-warehouse (pallet → pallet)."
    )]
    Haul {
        #[command(subcommand)]
        action: Option<HaulAction>,
    },

    /// Print the command list, or detailed help about one command
    #[command(visible_alias = "h")]
    Help {
        /// The command to explain (subcommands work too, e.g. "help office admit")
        #[arg(value_name = "COMMAND")]
        command: Vec<String>,
    },

    /// Print the parcels of a revision, newest first
    #[command(
        visible_alias = "hi",
        alias = "log",
        long_about = "Print the parcels of the current pallet (or of the given revision — a pallet \
                      name or a parcel hash), newest first: hash, operators, timestamps and \
                      description."
    )]
    History {
        /// A pallet name or a parcel hash prefix (default: the current pallet)
        revision: Option<String>,

        /// Show only parcels authored by this identity class: human | agent | bot | service
        #[arg(long)]
        class: Option<String>,

        /// Show at most this many parcels (newest first) — a bounded walk that never loads
        /// the whole history
        #[arg(long, short = 'n')]
        limit: Option<usize>,

        /// Resume paging from a cursor returned as `next` by a previous `--json` page
        /// (an opaque list of frontier parcels). Use with `-n` to read history in batches.
        #[arg(long, value_name = "CURSOR")]
        after: Option<String>,

        /// One line per parcel: the abbreviated hash and the description's first line only
        /// (git's `log --oneline`). Skips the author, timestamps and full message — and the
        /// office/display-name work behind them — so it is the fastest way to scan history.
        #[arg(long)]
        oneline: bool,
    },

    /// Import a git repository's history into this warehouse (one-way migration)
    #[command(
        long_about = "One-way migration of a git repository's history into this warehouse: \
                      git commits become parcels, trees become trees, blobs become blobs, and each \
                      local branch becomes a pallet. The imported history is unsigned (it predates \
                      trust), so import into a fresh warehouse and then run \"office enroll\" to \
                      anchor it. Run it from the git repo's directory (\"forklift import-git .\") so \
                      the checked-out working tree matches the imported HEAD. Requires git on PATH."
    )]
    ImportGit {
        /// The git repository to import from (e.g. \".\")
        path: String,

        /// Leave the imported objects loose (skip the automatic post-import compaction)
        #[arg(long)]
        no_compact: bool,
    },

    /// Upload the current pallet's new parcels to the remote (push)
    #[command(
        visible_alias = "li",
        alias = "push",
        long_about = "Upload the current pallet's new parcels to the configured remote \
                      (\"remote.url\") and move the remote's ref. When trust is established, the \
                      office pallet and the trust anchor are lifted first — the remote verifies \
                      every signature before it accepts the update. A remote that is ahead or has \
                      diverged is never overwritten: lower first."
    )]
    Lift,

    /// Take the current state of a file or directory into the inventory (stage)
    #[command(visible_alias = "l", alias = "add")]
    Load {
        /// The file or directory to load
        path: String,
    },

    /// Download the remote's new parcels and fast-forward the current pallet (pull)
    #[command(
        visible_alias = "lo",
        alias = "pull",
        long_about = "Download the remote's new parcels for the current pallet from the configured \
                      remote (\"remote.url\") and fast-forward to them — working directory and \
                      inventory included. On first contact with a trusted remote its trust anchor \
                      is adopted (one-way, like \"office enroll\"). A diverged pallet is reported, \
                      never merged implicitly."
    )]
    Lower,

    /// Attach signed post-metadata to a parcel: approvals, review notes (`mf`)
    #[command(
        visible_alias = "mf",
        long_about = "Record and read the manifest — signed post-metadata attached to a parcel \
                      after the fact (approvals, review notes; later, machine-authorship \
                      provenance). Entries are tracked metadata on the \"@manifest\" meta pallet: \
                      forge-proof, portable and auditable, and they reference parcels without ever \
                      mutating them. Recording needs an enrolled signing key (like the office). \
                      Without a subcommand, the whole manifest is listed."
    )]
    Manifest {
        #[command(subcommand)]
        action: Option<ManifestAction>,
    },

    /// Serve the command surface to an AI agent as MCP tools (stdio)
    #[command(
        long_about = "Run a Model Context Protocol server on stdin/stdout, \
                      exposing forklift's commands as schema-typed MCP tools so an agent calls \
                      tools instead of shelling out and scraping prose. Each tool re-runs forklift \
                      with --json, so the tools return the same structured output the CLI does. \
                      Point an MCP client (e.g. an AI coding tool) at \"forklift mcp\" with the \
                      warehouse as the working directory, or pass --root to target one directly."
    )]
    Mcp {
        /// The warehouse to serve (default: the current directory, discovered upward)
        #[arg(long)]
        root: Option<String>,
    },

    /// Manage the warehouse office: users and signing keys
    #[command(
        visible_alias = "o",
        long_about = "Manage the warehouse office: users and signing keys, tracked as metadata on \
                      the reserved \"office\" pallet. Without a subcommand, the users and their \
                      keys are listed."
    )]
    Office {
        #[command(subcommand)]
        action: Option<OfficeAction>,
    },

    /// Create a new pallet (branch) and shift to it; without a name, list the pallets
    #[command(
        visible_alias = "pz",
        alias = "branch",
        long_about = "Create a new pallet at the current head — or at the given revision (a pallet \
                      name or a parcel hash) — and shift to it. Without arguments, the pallets are \
                      listed with the current one marked."
    )]
    Palletize {
        /// The name of the new pallet; omit it to list the pallets
        name: Option<String>,

        /// Where the new pallet starts: a pallet name or a parcel hash prefix
        /// (default: the current pallet's head)
        revision: Option<String>,

        /// When listing, also show the meta pallets (the office and other tracked
        /// metadata; reach them with the "@" qualifier, e.g. "history @office")
        #[arg(short, long)]
        all: bool,
    },

    /// Park the work in progress and reset to the pallet head
    #[command(
        visible_alias = "pa",
        alias = "stash",
        long_about = "Park the work in progress (the staged and unstaged changes of tracked files) \
                      as a parked parcel and reset the warehouse to the pallet head. Untracked \
                      files are left alone. \"park pop\" re-applies the latest parked changes; \
                      \"park list\" lists them."
    )]
    Park {
        #[command(subcommand)]
        action: Option<ParkAction>,
    },

    /// Peek into an object (print its content)
    #[command(
        visible_alias = "pk",
        long_about = "Print the content of an object: a blob's bytes, a tree's entries, or a \
                      parcel's fields. With --inventory, print the inventory of a working-directory \
                      folder instead."
    )]
    Peek {
        /// Peek into the inventory of a working-directory folder instead of an object
        #[arg(short, long, value_name = "PATH", conflicts_with = "object")]
        inventory: Option<String>,

        /// The hash of the object to peek into
        #[arg(value_name = "HASH", required_unless_present = "inventory")]
        object: Option<String>,
    },

    /// Prepare a new warehouse in the current directory
    #[command(
        visible_alias = "p",
        alias = "init",
        long_about = "Prepare a new warehouse in the current directory: the \".forklift\" folder \
                      with the object store, the staging area, the configuration template and the \
                      ignore file. Preparing is idempotent — only the missing pieces are created."
    )]
    Prepare,

    /// Manage identity profiles (named operator ids) and select one per warehouse
    #[command(
        long_about = "Manage identity profiles: named identity bundles (an operator id and a \
                      display name) stored in the global configuration. A warehouse selects one \
                      with \"profile use\", so one machine can act as different operators in \
                      different warehouses — e.g. a personal identity and one per organization. \
                      Without a subcommand, the profiles are listed."
    )]
    Profile {
        #[command(subcommand)]
        action: Option<ProfileAction>,
    },

    /// Restore a file or directory from the inventory (discard unstaged changes)
    #[command(
        visible_alias = "r",
        long_about = "Restore a file or directory in the working directory from the inventory \
                      (discard unstaged changes). With --staged, reset the inventory entries to \
                      the pallet head instead (unstage), leaving the working directory untouched."
    )]
    Restore {
        /// Reset the inventory entries to the pallet head instead (unstage)
        #[arg(short, long)]
        staged: bool,

        /// The file or directory to restore
        path: String,
    },

    /// Check for a newer release and update this binary (client only)
    #[command(
        long_about = "Check the latest published release against this binary and, for a \
                      script/manual install, update in place by re-running the verified install \
                      script. A package-manager install (cargo/brew) is never overwritten — \
                      self-update shows the right upgrade command instead. There is deliberately \
                      no server self-update: a server is redeployed, not self-mutated."
    )]
    SelfUpdate {
        /// Only report whether an update is available and how to get it; change nothing
        #[arg(long)]
        check: bool,
    },

    /// Shift to another pallet
    #[command(
        visible_alias = "sh",
        aliases = ["checkout", "switch"],
        long_about = "Shift to another pallet: materialize its head parcel in the working \
                      directory and repopulate the inventory from it. Refuses to run while there \
                      are staged or unstaged changes (untracked files are tolerated)."
    )]
    Shift {
        /// The pallet to shift to
        pallet: String,
    },

    /// Stack the inventory as a new parcel (commit) on the current pallet
    #[command(
        visible_alias = "s",
        alias = "commit",
        long_about = "Stack the inventory as a new parcel (commit) on the current pallet, with the \
                      configured operator recorded as the author. A consolidation in progress is \
                      completed by this command."
    )]
    Stack {
        /// The description of the parcel
        description: Option<String>,
    },

    /// Report the staged and unstaged changes of the warehouse
    #[command(
        visible_alias = "st",
        alias = "status",
        long_about = "Report the current pallet, the staged changes (inventory vs pallet head — \
                      what the next \"stack\" records) and the changes not yet loaded (working \
                      directory vs inventory). With --summary, report only the counts (a \
                      token-cheap overview for scripts and agents)."
    )]
    Stocktake {
        /// Report only the counts, not the per-path changes
        #[arg(long)]
        summary: bool,
    },

    /// Widen a sparse warehouse's fetch scope and download the newly in-scope subtree(s)
    #[command(
        long_about = "Widen a sparse warehouse's fetch scope: add subtree path(s) to what the \
                      store fetches, then download their content across the whole history from the \
                      remote. Cheap and incremental — only the newly in-scope objects are fetched; \
                      what is already present is skipped. After expanding, a bay can be scoped to \
                      the new path (\"bay add --scope\"). A full (non-sparse) warehouse already \
                      holds everything, so there is nothing to expand."
    )]
    Expand {
        /// The subtree path(s) to add to the fetch scope. Repeatable.
        #[arg(value_name = "PATH", required = true)]
        paths: Vec<String>,
    },

    /// Narrow this checkout's materialization scope and de-materialize the dropped subtree(s)
    #[command(
        long_about = "Narrow this checkout's materialization scope: drop subtree path(s) from what \
                      this bay (or the main tree) materializes, and remove the now out-of-scope \
                      files from the working directory. This frees nothing in the shared object \
                      store — the dropped content is still ordinary reachable history, not garbage \
                      — it only shrinks what this checkout shows. A checkout must keep at least one \
                      in-scope path; to stop scoping entirely, open a fresh full checkout."
    )]
    Narrow {
        /// The in-scope subtree path(s) to drop. Repeatable.
        #[arg(value_name = "PATH", required = true)]
        paths: Vec<String>,
    },

    /// Show this bay's sparse materialization scope, and the warehouse fetch scope
    #[command(
        long_about = "Show the sparse-workspace scope: this bay's materialization scope \
                      (the subtree path(s) it checks out, stages and stacks) and the warehouse's \
                      fetch scope (what the store has fetched at all). An \
                      unscoped bay (or the main tree) reports a full scope: the whole tree. Scope \
                      is local to the checkout and never tracked. Read-only."
    )]
    Scope {
        #[command(subcommand)]
        action: Option<ScopeAction>,
    },

    /// Report object-store health: loose vs packed objects, packs, and whether compaction is due
    #[command(
        long_about = "Report the health of the object store — the counterpart of \"stocktake\" \
                      (which reports the working tree). Shows how many objects are loose \
                      (unpacked) versus packed, how many pack files there are and how \
                      delta-dense they are, the on-disk sizes, and whether an incremental \
                      compaction or a consolidating repack is currently due per the \
                      maintenance.loose / maintenance.packs thresholds. Read-only; run \
                      \"compact\" to act on it."
    )]
    Store,

    /// Manage signed tags: named, admin-signed pointers at a parcel (releases)
    #[command(
        long_about = "Manage tags: named, signed pointers at a parcel — releases and \
                      milestones. A tag is a signed record on the \"@tags\" meta pallet, so who \
                      cut it is the parcel's signature (forge-proof), verifiable offline against \
                      the office chain. The release convention: a tag is signed by an admin key, \
                      so creating one requires an admin. Tag names are immutable. Without a \
                      subcommand, the tags are listed."
    )]
    Tag {
        #[command(subcommand)]
        action: Option<TagAction>,
    },

    /// Undo the last stack on the current pallet (soft — keeps your changes)
    #[command(
        long_about = "Undo the last \"stack\" on the current pallet: move the pallet head back to \
                      the previous parcel, keeping the working directory and the inventory as they \
                      are (like git's \"reset --soft HEAD~1\"). The undone parcel's changes are \
                      staged again, so you can re-stack them — e.g. with a corrected message or \
                      author — or adjust them first. The parcel is not deleted, only taken off the \
                      pallet. Merge parcels (completed consolidations) are not undone yet."
    )]
    Undo,

    /// Stage a file or directory for removal
    #[command(
        visible_alias = "ul",
        long_about = "Stage a file or directory for removal: its inventory entries are marked as \
                      deleted, so they will not be part of the next parcel. The working directory \
                      is not touched."
    )]
    Unload {
        /// The file or directory to unload
        path: String,
    },

    /// Print the version of Forklift
    #[command(visible_alias = "v")]
    Version,

}

#[derive(Subcommand)]
pub enum AliasAction {
    /// Create the alias next to this binary (default name: "fl")
    #[command(
        long_about = "Create the alias next to this binary: on Unix, a symlink; on Windows \
                      (where a symlink needs elevated privilege), a `.cmd` shim that forwards to \
                      this binary. Resolves this binary's own path (`current_exe`), so it always \
                      points at whichever `forklift` is actually running the command. Idempotent: \
                      an alias that already points here succeeds without change. Refuses — without \
                      a --force, there isn't one — if the name is already taken by something else \
                      (a real file, or a symlink to a different target): removing an unrelated \
                      file automatically would be a surprise no flag should paper over."
    )]
    Install {
        /// The alias name (default: "fl")
        name: Option<String>,
    },

    /// Remove the alias next to this binary (default name: "fl")
    #[command(
        long_about = "Remove the alias next to this binary. A no-op (success) if it is not \
                      installed. Refuses if the name exists but was not created by this command \
                      (a real file, e.g.). A symlink (Unix) or a recognized shim (Windows) is \
                      removed even if it points at a different forklift binary — deleting a \
                      symlink can never lose data, unlike a real file."
    )]
    Uninstall {
        /// The alias name (default: "fl")
        name: Option<String>,
    },

    /// Report whether the alias is installed and where it points (the default)
    Status,
}

#[derive(Subcommand)]
pub enum BayAction {
    /// Open a new bay (a new working directory on a new pallet)
    #[command(
        long_about = "Open a bay: a new working directory bound to this warehouse, checked out to \
                      a new pallet named after the bay (branched from the current head). The bay \
                      shares this warehouse's objects, refs and trust; it has its own working tree, \
                      inventory and current pallet. cd into it to work there."
    )]
    Add {
        /// The bay's name (also the name of the new pallet it checks out)
        name: String,

        /// Where to create the working directory (default: a sibling of the warehouse)
        path: Option<String>,

        /// Limit the bay to one or more subtree paths — a scoped (sparse) bay. Repeatable.
        #[arg(
            long = "scope",
            value_name = "PATH",
            long_help = "Open a scoped (sparse) bay: materialize and operate on only the \
                         given subtree path(s) of the working tree, not the whole tree. On a full \
                         warehouse the object store still holds everything (only materialization is \
                         scoped); the bay just checks out, stages and stacks its in-scope \
                         subtree(s), copying every out-of-scope sibling forward by the hash the \
                         signed head already commits. A bay's scope must be within what the \
                         warehouse fetched (see \"franchise --only\" and \"expand\"). Repeatable to \
                         scope several subtrees. Scope is local to this bay and never tracked."
        )]
        scope: Vec<String>,
    },

    /// De-register a bay (its pallet ref and materialized files are kept)
    #[command(
        long_about = "De-register a bay: remove its local state and the redirect in its working \
                      directory. The bay's pallet is a normal ref and is kept, and its materialized \
                      files are left in place — delete the directory yourself to reclaim the space."
    )]
    Remove {
        /// The bay to remove
        name: String,
    },
}

#[derive(Subcommand)]
pub enum ScopeAction {
    /// Show the current bay's materialization scope and the warehouse fetch scope (the default)
    Status,
}

#[derive(Subcommand)]
pub enum TagAction {
    /// Create a signed tag pointing at a revision (admin only)
    #[command(
        long_about = "Create a signed tag pointing at a revision — a release marker. The tag is \
                      signed and recorded on the \"@tags\" meta pallet, verifiable offline against \
                      the office chain. As the release convention, only an admin may cut a \
                      tag. Tag names are immutable; an existing name is refused."
    )]
    Create {
        /// The tag name (a release label, e.g. "v1.2.0")
        name: String,

        /// The revision to tag: a pallet name or a parcel hash (default: the current pallet head)
        revision: Option<String>,

        /// The tag message
        #[arg(short = 'm', long)]
        message: Option<String>,
    },

    /// Show one tag in full (subject, tagger, message)
    Show {
        /// The tag name
        name: String,
    },

    /// List every tag (the default)
    List,
}

#[derive(Subcommand)]
pub enum ManifestAction {
    /// Attach a free-form note to a parcel
    #[command(
        long_about = "Record a signed note about a parcel — a review comment, a caveat, any \
                      free-form annotation. The note references the parcel and never changes it. \
                      Requires an enrolled signing key."
    )]
    Note {
        /// The parcel the note is about (a pallet name, or a parcel hash / unique prefix)
        revision: String,

        /// The note text
        #[arg(short = 'm', long)]
        message: String,
    },

    /// Record a signed approval (sign-off) of a parcel
    #[command(
        long_about = "Record a signed approval of a parcel — a cryptographic sign-off, forge-proof \
                      and offline-verifiable. Optionally attach a message. Requires an enrolled \
                      signing key."
    )]
    Approve {
        /// The parcel to approve (a pallet name, or a parcel hash / unique prefix)
        revision: String,

        /// An optional message to attach to the approval
        #[arg(short = 'm', long)]
        message: Option<String>,
    },

    /// Record how a parcel was produced: model, tool, session (AI provenance)
    #[command(
        long_about = "Record signed machine-authorship provenance for a parcel — which model \
                      produced the change, with which tool, in which session, and (optionally) a \
                      transcript fingerprint. Because the entry is signed, it is evidence: paired \
                      with an agent-class identity (\"office admit --agent --supervisor\"), it \
                      answers \"which model produced this, under whose supervision\" forge-proof \
                      and offline — the AI-traceability question git cannot answer. Requires an \
                      enrolled signing key."
    )]
    Provenance {
        /// The parcel the provenance is about (a pallet name, or a parcel hash / prefix)
        revision: String,

        /// The model that produced the change (e.g. claude-opus-4-8)
        #[arg(long)]
        model: String,

        /// The tool or agent that ran the model (e.g. claude-code)
        #[arg(long)]
        tool: Option<String>,

        /// The session / conversation id the change came from
        #[arg(long)]
        session: Option<String>,

        /// A hash or fingerprint of the prompt / transcript
        #[arg(long)]
        transcript: Option<String>,

        /// An optional human-readable summary
        #[arg(short = 'm', long)]
        message: Option<String>,
    },

    /// Show the manifest entries attached to a parcel
    #[command(
        long_about = "Show every manifest entry (approvals, notes) attached to a parcel, oldest \
                      first, with who recorded each and when."
    )]
    Show {
        /// The parcel whose manifest to show (a pallet name, or a parcel hash / prefix)
        revision: String,
    },
}

#[derive(Subcommand)]
pub enum HaulAction {
    /// Open a merge proposal
    Open {
        /// The pallet to merge into
        #[arg(long)]
        target: String,

        /// The pallet to merge from (default: the current pallet)
        #[arg(long)]
        source: Option<String>,

        /// A short title for the proposal
        #[arg(long)]
        title: String,

        /// A longer description
        #[arg(short = 'm', long)]
        message: Option<String>,
    },

    /// List proposals
    List {
        /// Which to show: open (default) | merged | closed | all
        #[arg(long)]
        state: Option<String>,
    },

    /// Show one proposal in full
    Show {
        /// The haul id (or a unique prefix)
        id: String,
    },

    /// Add a comment
    Comment {
        /// The haul id (or a unique prefix)
        id: String,

        /// The comment text
        #[arg(short = 'm', long)]
        message: String,
    },

    /// Record a signed review (approve by default)
    #[command(
        long_about = "Record a signed review of a haul. Approves by default; pass --request-changes \
                      or --comment for the other verdicts. Because the review is signed, the approval \
                      carries your identity class — an agent's approval is distinguishable from a \
                      human's. Requires an enrolled signing key."
    )]
    Review {
        /// The haul id (or a unique prefix)
        id: String,

        /// Request changes instead of approving
        #[arg(long, conflicts_with = "comment")]
        request_changes: bool,

        /// A plain review comment (no approval)
        #[arg(long)]
        comment: bool,

        /// The review message
        #[arg(short = 'm', long)]
        message: Option<String>,
    },

    /// Merge a proposal (consolidate its source into the target)
    Merge {
        /// The haul id (or a unique prefix)
        id: String,
    },

    /// Close a proposal without merging
    Close {
        /// The haul id (or a unique prefix)
        id: String,
    },

    /// Reopen a closed proposal
    Reopen {
        /// The haul id (or a unique prefix)
        id: String,
    },
}

#[derive(Subcommand)]
pub enum OfficeAction {
    /// Enroll the configured operator and establish trust — one-way!
    #[command(
        long_about = "Enroll the configured operator and establish trust: the genesis office \
                      parcel introduces the first user and key. This is a one-way door — from \
                      then on every parcel stacked in this warehouse must be signed. When a \
                      remote is configured, its pallet heads are included in the trust boundary \
                      (history the remote already has stays valid unsigned), so the remote must \
                      be reachable — or pass --offline to skip it."
    )]
    Enroll {
        /// Skip the configured remote (its heads will not be in the trust boundary)
        #[arg(
            long,
            long_help = "Establish trust without consulting the configured remote. Only for \
                         when the remote is gone for good: unsigned history that only the \
                         remote has would fall outside the trust boundary, and lifting to it \
                         would be refused forever."
        )]
        offline: bool,

        /// Protect the new key with a passphrase (recommended for human operators)
        #[arg(
            long,
            long_help = "Encrypt the new private key with a passphrase (prompted). A protected \
                         key cannot be used by a process that lacks the passphrase — even one \
                         running as you — so an unattended agent cannot sign as you. Leave it \
                         off for passphraseless machine identities (agents, bots, CI)."
        )]
        passphrase: bool,
    },

    /// Generate a keypair locally and print its public half + proof-of-possession
    #[command(
        long_about = "Generate a keypair locally and print its public half plus a \
                      proof-of-possession (a self-signature binding the key to your operator \
                      UUID). The private key stays on this machine. Hand the printed admit line \
                      to an office admin to be enrolled, or the printed link line to a machine \
                      that already holds one of your keys to add this as another device."
    )]
    Keygen {
        /// Protect the new key with a passphrase (recommended for human operators)
        #[arg(long)]
        passphrase: bool,
    },

    /// Enroll another operator with their public key — admins only
    #[command(
        long_about = "Enroll another operator: their UUID, public key and proof-of-possession, \
                      exactly as printed by their \"forklift office keygen\" run. The key becomes \
                      their identity root, endorsed by your admin key. Office records are \
                      pseudonymous — no names or emails ever go on-chain. Mark automated \
                      principals with --agent / --bot / --service: the class rides in the \
                      signed record, so \"an agent wrote this, supervised by <human>\" is \
                      forge-proof. An agent requires a --supervisor; a supervisor must be an \
                      enrolled human. Automated identities should hold passphraseless keys (they \
                      sign autonomously under their own marked identity, never as a human)."
    )]
    Admit {
        /// The operator's UUID (printed by their "office keygen")
        #[arg(value_name = "OPERATOR_UUID")]
        operator_id: String,

        /// The operator's public key (64 hex characters)
        #[arg(value_name = "PUBLIC_KEY_HEX")]
        public_key: String,

        /// The key's proof-of-possession (printed by their "office keygen")
        #[arg(value_name = "PROOF_OF_POSSESSION_HEX")]
        pop: String,

        /// The operator's role: admin, writer or reader
        #[arg(long, default_value = "writer")]
        role: String,

        /// Restrict a writer to these pallets (repeatable; default: all pallets)
        #[arg(long = "pallet", value_name = "PALLET")]
        pallets: Vec<String>,

        /// Mark this operator as an AI agent (requires --supervisor)
        #[arg(long, conflicts_with_all = ["bot", "service"])]
        agent: bool,

        /// Mark this operator as an automation bot
        #[arg(long, conflicts_with_all = ["agent", "service"])]
        bot: bool,

        /// Mark this operator as a service / CI identity
        #[arg(long, conflicts_with_all = ["agent", "bot"])]
        service: bool,

        /// The supervising human operator (required for --agent)
        #[arg(long, value_name = "OPERATOR")]
        supervisor: Option<String>,
    },

    /// Link another device's key to your own identity
    #[command(
        long_about = "Link another device's key to your own identity: a sigchain endorsement — \
                      the new key is trusted because a key you already hold signs for it, so \
                      every warehouse that trusts your identity accepts it automatically. Run \
                      \"forklift office keygen\" on the other device (configured with the same \
                      operator.uuid) and pass the printed public key and proof-of-possession \
                      here."
    )]
    Link {
        /// The new key's public half (64 hex characters)
        #[arg(value_name = "PUBLIC_KEY_HEX")]
        public_key: String,

        /// The new key's proof-of-possession (printed by "office keygen")
        #[arg(value_name = "PROOF_OF_POSSESSION_HEX")]
        pop: String,
    },

    /// Authorize a new key for an already-enrolled operator — admins only
    #[command(
        long_about = "Authorize a new key for an already-enrolled operator: the recovery path for \
                      someone who lost every device (the scope of a key-authorization \
                      equals the scope of the authorizer's authority, so an admin's endorsement \
                      is valid exactly here, in the office they administer). The operator runs \
                      \"forklift office keygen\" on their new machine and hands you the printed \
                      line over a channel where you can confirm it is really them; their \
                      proof-of-possession still co-signs the record, so you cannot attribute a \
                      key they do not hold. For your own devices use \"office link\"; for a new \
                      operator use \"office admit\"."
    )]
    Authorize {
        /// The enrolled operator's UUID (printed by their "office keygen")
        #[arg(value_name = "OPERATOR_UUID")]
        operator_id: String,

        /// The new key's public half (64 hex characters)
        #[arg(value_name = "PUBLIC_KEY_HEX")]
        public_key: String,

        /// The new key's proof-of-possession (printed by "office keygen")
        #[arg(value_name = "PROOF_OF_POSSESSION_HEX")]
        pop: String,
    },

    /// Change a user's role (and a writer's pallet grants) — admins only
    #[command(
        long_about = "Change a user's role (admin, writer or reader) and, for writers, the pallet \
                      grants. Admins only, with lockout protection: the office always keeps at \
                      least one admin. Remotes enforce roles on every lift."
    )]
    Role {
        /// The identifier of the user
        identifier: String,

        /// The new role: admin, writer or reader
        role: String,

        /// Restrict a writer to these pallets (repeatable; default: all pallets)
        #[arg(long = "pallet", value_name = "PALLET")]
        pallets: Vec<String>,
    },

    /// Issue a fresh key for the configured operator and retire the old ones
    #[command(
        long_about = "Issue a fresh key for the configured operator and retire the old ones. The \
                      change is signed with the old key, proving the rotation was authorized by \
                      its owner. Each retirement records a distrust boundary (the pallet heads \
                      vouched for right now, the remote's included), so a retired key cannot \
                      silently sign anything new."
    )]
    Rotate {
        /// Skip the configured remote (its heads will not join the distrust boundary)
        #[arg(long)]
        offline: bool,

        /// Protect the new key with a passphrase (recommended for human operators)
        #[arg(long)]
        passphrase: bool,
    },

    /// Revoke a key: retire it, or mark it compromised
    #[command(
        long_about = "Revoke a key. Plain retirement is the routine goodbye (a decommissioned \
                      machine); --compromised marks a key that may be in someone else's hands. \
                      Either way the revocation records a distrust boundary — the pallet heads \
                      vouched for right now, the remote's included: signatures by the key outside \
                      that ancestry fail every future audit. Exact ancestry, never timestamps, so \
                      a shifted clock cannot forge validity."
    )]
    Retire {
        /// The id of the key to revoke
        key_id: String,

        /// The key may be in someone else's hands (recorded as the revocation reason)
        #[arg(long)]
        compromised: bool,

        /// Skip the configured remote (its heads will not join the distrust boundary)
        #[arg(long)]
        offline: bool,
    },

    /// RESET trust: new root, prior history pinned as attested — recovery only
    #[command(
        long_about = "Re-genesis: the recovery primitive for a chain nobody can extend anymore \
                      (all keys lost, no admin left). Creates a new self-endorsed trust root for \
                      the configured operator and replaces the trust anchor; the old office head \
                      is pinned in the new anchor as attested history (kept, but its guarantee \
                      degrades from verified to attested). LOUD: every clone refuses to sync \
                      until its holder consciously re-accepts, and a remote accepts the reset \
                      only from the server operator's static token. Without --confirm, prints \
                      what would happen."
    )]
    Regenesis {
        /// Actually perform the reset (without it, a dry-run explanation is printed)
        #[arg(long)]
        confirm: bool,
    },

    /// Consciously accept a remote's trust reset (re-genesis)
    #[command(
        name = "accept-regenesis",
        long_about = "Accept a remote's re-genesis: after a trust reset every sync is refused \
                      until you deliberately accept the new anchor — the SSH host-key-change \
                      moment. The new anchor must name your pinned genesis as its prior (the \
                      chain of custody); verify out-of-band that the reset is legitimate before \
                      confirming. Without --confirm, shows what would be accepted."
    )]
    AcceptRegenesis {
        /// Actually accept the new anchor (without it, the reset is only described)
        #[arg(long)]
        confirm: bool,
    },

    /// List the users and their keys (the default)
    List,
}

#[derive(Subcommand)]
pub enum ProfileAction {
    /// List the profiles and the local keys each one holds (the default)
    List,

    /// Create a named profile (mints an operator id unless one is given)
    Create {
        /// The profile name (e.g. "work")
        name: String,

        /// The display name (local only; falls back to the operator id)
        #[arg(long = "name", value_name = "DISPLAY_NAME")]
        display_name: Option<String>,

        /// The operator id (e.g. one issued by a hosting provider); minted when omitted
        #[arg(long = "id", value_name = "OPERATOR_ID")]
        identifier: Option<String>,
    },

    /// Act as this profile in the current warehouse
    Use {
        /// The profile name
        name: String,
    },
}

#[derive(Subcommand)]
pub enum ParkAction {
    /// Re-apply the most recently parked changes (staged) and drop them from the list
    Pop,

    /// List the parked parcels, newest first
    List,
}

impl Command {
    /// Check whether the command operates on an existing warehouse.
    /// For these commands, the warehouse root is discovered (walking up from the current
    /// directory) and the process switches to it before the command runs.
    ///
    /// `config` is missing deliberately: it enters the warehouse itself, and only when
    /// the warehouse scope is actually involved (global reads and writes must work
    /// outside a warehouse).
    ///
    /// # Returns
    /// * `true`  - If the command requires an existing warehouse.
    /// * `false` - If the command can run anywhere (e.g. `prepare`, `help`).
    pub fn requires_warehouse(&self) -> bool {
        matches!(
            self,
            Command::Audit { .. }
                | Command::Bay { .. }
                | Command::Blame { .. }
                | Command::CherryPick { .. }
                | Command::Compact { .. }
                | Command::Conflicts
                | Command::Consolidate { .. }
                | Command::Deliver { .. }
                | Command::Diff { .. }
                | Command::Expand { .. }
                | Command::ExportGit { .. }
                | Command::History { .. }
                | Command::ImportGit { .. }
                | Command::Lift
                | Command::Load { .. }
                | Command::Lower
                | Command::Haul { .. }
                | Command::Manifest { .. }
                | Command::Narrow { .. }
                | Command::Office { .. }
                | Command::Palletize { .. }
                | Command::Park { .. }
                | Command::Peek { .. }
                | Command::Restore { .. }
                | Command::Scope { .. }
                | Command::Shift { .. }
                | Command::Stack { .. }
                | Command::Stocktake { .. }
                | Command::Store
                | Command::Tag { .. }
                | Command::Undo
                | Command::Unload { .. }
        )
    }

    /// Check whether the command mutates the warehouse (inventory, refs, objects) and
    /// therefore has to hold the warehouse lock while it runs (see `WarehouseLock`).
    ///
    /// # Returns
    /// * `true`  - If the command must run under the warehouse lock.
    /// * `false` - If the command is read-only (or does not touch a warehouse at all).
    pub fn requires_warehouse_lock(&self) -> bool {
        matches!(
            self,
            Command::Bay { .. }
                | Command::CherryPick { .. }
                | Command::Compact { .. }
                | Command::Consolidate { .. }
                | Command::Deliver { .. }
                | Command::Expand { .. }
                | Command::ImportGit { .. }
                | Command::Load { .. }
                | Command::Lower
                | Command::Haul { .. }
                | Command::Manifest { .. }
                | Command::Narrow { .. }
                | Command::Office { .. }
                | Command::Palletize { .. }
                | Command::Park { .. }
                | Command::Restore { .. }
                | Command::Shift { .. }
                | Command::Stack { .. }
                | Command::Tag { .. }
                | Command::Undo
                | Command::Unload { .. }
        )
    }

    /// Whether finishing this command should kick off background object-store maintenance
    /// (git's `gc --auto`): the mutating commands that add loose objects, so the store stays
    /// packed without the user remembering to `compact`. `compact` itself is excluded (or it
    /// would re-trigger), and `import-git` is (it compacts on its own). The actual work is
    /// still gated on a cheap loose/pack threshold, so triggering here is only a *check*.
    ///
    /// # Returns
    /// * `true`  - Check for (and maybe spawn) background maintenance after this command.
    /// * `false` - Do not (a read-only, setup, or self-compacting command).
    pub fn triggers_auto_maintenance(&self) -> bool {
        self.requires_warehouse_lock()
            && !matches!(self, Command::Compact { .. } | Command::ImportGit { .. })
    }

    /// Whether the command produces read-only, potentially long output that should go
    /// through a pager on a terminal. Git pages its log/diff/show/blame family and never a
    /// command that mutates or might prompt (a passphrase behind a pager would deadlock), so
    /// this is exactly the read-only display commands.
    ///
    /// # Returns
    /// * `true`  - Page the output on a terminal (`history`, `diff`, `peek`, `blame`, `audit`).
    /// * `false` - Print straight through (mutating, interactive, or short-output commands).
    pub fn pages_output(&self) -> bool {
        matches!(
            self,
            Command::History { .. }
                | Command::Diff { .. }
                | Command::Peek { .. }
                | Command::Blame { .. }
                | Command::Audit { .. }
        )
    }

    /// The name this command records in the undo journal (§7.8), or `None` if it is not
    /// journaled. Only state-changing operations `undo` knows how to reverse are listed;
    /// their pre-operation state is snapshotted so `undo` can restore it. Pure-staging
    /// (`load`/`unload`/`restore`), trust and remote commands are intentionally excluded.
    pub fn journal_op(&self) -> Option<&'static str> {
        match self {
            Command::Stack { .. } => Some("stack"),
            Command::Consolidate { .. } => Some("consolidate"),
            Command::CherryPick { .. } => Some("cherry-pick"),
            Command::Shift { .. } => Some("shift"),
            _ => None,
        }
    }
}

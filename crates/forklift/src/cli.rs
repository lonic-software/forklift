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
        long_help = "Emit the command's result as one JSON document on stdout instead of prose, \
                     usage errors included. The header is \
                     { \"lonic_json\": 1, \"tool\": \"forklift\", \"schema\", \"command\", \"ok\" }, \
                     followed by \"data\" on success or \"error\": { \"code\", \"message\", \
                     \"next_step\"?, \"details\"? } on failure. Errors also set a deterministic \
                     exit code."
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

    /// When to color output: auto (on a terminal), always, or never.
    #[arg(
        long,
        global = true,
        value_enum,
        value_name = "WHEN",
        default_value_t = crate::color::ColorChoice::Auto,
        long_help = "When to color human output. auto, the default, colors only when stdout is a \
                     terminal, NO_COLOR is unset or empty, and TERM is set and not `dumb` (on \
                     Windows, TERM may be unset), so a pipe or a file gets plain text. always and \
                     never override all of these. Paged output keeps its color; help and usage \
                     errors follow the flag too."
    )]
    pub color: crate::color::ColorChoice,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {

    /// Verify the warehouse's signed history offline
    #[command(
        long_about = "Verify the warehouse's signed history offline: the office chain back to the \
                      genesis parcel, then the given pallet (the current one by default). Requires \
                      established trust (\"forklift office enroll\").\n\n\
                      By default, large chunked files are presence-checked (their chunks are \
                      confirmed present without re-reading the bytes). \"--full\" re-reads every \
                      present chunk — re-hashing it — and re-assembles each chunked file to confirm \
                      its recorded content hash; a stronger, slower pass."
    )]
    Audit {
        /// The pallet to audit (default: the current pallet)
        pallet: Option<String>,

        /// Re-read and re-hash every present chunk, and re-assemble each chunked file to verify its
        /// content hash (a normal audit only presence-checks chunks)
        #[arg(long)]
        full: bool,
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

    /// Clone a remote warehouse into a new directory
    #[command(
        long_about = "Clone a remote warehouse: prepare a fresh warehouse in \
                      the target directory, remember the remote, adopt its trust anchor, download \
                      the history (using the remote's bundle when it has one) and materialize the \
                      chosen pallet in the working directory. With --only, clone sparsely: \
                      fetch the full signed history but only the named subtree(s) of content, \
                      leaving the rest sealed by hash. With --cache, mark the warehouse as a \
                      cache, which may lack content its history reaches."
    )]
    Clone {
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

        /// Clone sparsely: fetch only the named subtree path(s) of content. Repeatable.
        #[arg(
            long = "only",
            value_name = "PATH",
            long_help = "Clone sparsely: fetch the whole signed history — every parcel, \
                         signature and the tree spine — but only the content under the named \
                         subtree path(s). Out-of-scope subtrees and blobs are never downloaded; \
                         they stay pinned by the exact hash a signed parcel already commits, so \
                         nothing can be forged, it is simply not fetched. The working tree \
                         materializes only the in-scope subtree(s). The remote's whole-store \
                         bundle is skipped (it would defeat the point). Repeatable to fetch \
                         several subtrees. Widen later with \"forklift scope fetch\"."
        )]
        only: Vec<String>,

        /// Clone as a cache: a warehouse that may lack content its history reaches
        #[arg(
            long = "cache",
            conflicts_with = "only",
            long_help = "Clone as a cache: mark the warehouse as one that may lack content its \
                         history reaches. It is then treated as partial: a server never serves \
                         it, it lifts only to the remote it was cloned from, \"forklift git mirror\" and \
                         \"forklift scope fetch\" refuse, and \"forklift audit\" checks the content it holds. The \
                         remote's bundle is skipped. It fetches every parcel and signature of the \
                         history, and the content of the head it checks out; the content of \
                         older history stays with the remote, recorded by hash, and is fetched \
                         from it when a command reads it. Cannot be combined with --only."
        )]
        cache: bool,

        /// Refuse unless the remote's trust anchor is this genesis (an out-of-band pin)
        #[arg(
            long = "expect",
            value_name = "GENESIS",
            long_help = "Refuse unless the remote's trust anchor is exactly this genesis, a value \
                         obtained out of band (from an operator you know, or a first-contact \
                         warning you verified). Checked right after the handshake, before anything \
                         is downloaded or created. Without it, a first contact adopts whatever \
                         anchor the remote holds, and says which."
        )]
        expect: Option<String>,
    },

    /// Read or change configuration values
    #[command(
        long_about = "Read or change configuration values (e.g. \"remote.url\"): list every known \
                      key, get one, set one or unset one. Values are stored in the current \
                      warehouse by default, and each subcommand's --global targets the per-user \
                      configuration that applies to all warehouses; when reading, the warehouse value wins over the \
                      global one. Without a subcommand, every known key is listed with its \
                      effective value."
    )]
    Config {
        #[command(subcommand)]
        action: Option<ConfigAction>,
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

    /// Squash the current draft pallet onto a target as one clean parcel, keeping the trail
    #[command(
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
                      inventory (what \"forklift load\" would stage). With --staged, show the changes \
                      between the inventory and the pallet head (what \"forklift stack\" would record). \
                      With two revisions (pallet names or parcel hashes), compare those instead \
                      — use \":empty\" as either revision to compare against the empty tree \
                      (every file on the other side then lists as added). An optional trailing \
                      path limits the report to a file or directory."
    )]
    Diff {
        /// Compare the inventory against the pallet head (what "forklift stack" would record)
        #[arg(short, long)]
        staged: bool,

        /// [path], or <revision-a> <revision-b> [path] — ":empty" is the empty tree
        #[arg(value_name = "REVISION|PATH")]
        targets: Vec<String>,
    },

    /// Move history between this warehouse and git: import, export and mirror
    #[command(
        subcommand_required = true,
        arg_required_else_help = true,
        long_about = "Move history between this warehouse and git. \"forklift git import\" brings a \
                      git repository's history in, \"forklift git export\" writes this warehouse's \
                      history out to a new git repository, and \"forklift git mirror\" keeps a git \
                      remote in step with the pallets the Forklift remote holds. Each one requires \
                      git on PATH."
    )]
    Git {
        #[command(subcommand)]
        action: GitAction,
    },

    /// Reviewable merge proposals (pull requests) on the @haul meta pallet
    #[command(
        long_about = "Propose merging one pallet into another, with discussion and signed reviews — \
                      pull requests, in Forklift's vocabulary. A haul is a set of signed cells on the @haul \
                      meta pallet — its proposal, its proposed head, its status, one review per \
                      reviewer and its comments — so authorship, and who approved, is forge-proof \
                      and carries the operator's identity class (a human's approval is \
                      distinguishable from an agent's). A review names the head it reviewed, and a \
                      merged haul stays merged. Reviews are recorded; merging is not gated on them \
                      in this release. Intra-warehouse (pallet → pallet)."
    )]
    Haul {
        #[command(subcommand)]
        action: Option<HaulAction>,
    },

    /// Resolve a standing durability taint, or finish or undo an interrupted operation
    #[command(
        long_about = "Resolve a standing durability taint (\"durability_taint\" refusals, exit \
                      21): a write's final path(s) were left visible after a failed durability \
                      sync, and the automatic entry-heal every command runs could not restage \
                      every one of them on its own. This verb runs the deeper recovery pass — \
                      restage whatever it can, then walk every durable ref source (pallet heads, \
                      parked work, and staged inventory shards) to tell a vanished-but-\
                      never-referenced object (safe to drop) from one something still points at \
                      (reported, with remedies). A torn taint (its own record was cut short by a \
                      crash) is resolved by rescanning the whole store, which takes longer. \
                      Exits 0 once every recorded path is resolved; exits 21, with the taint left \
                      standing on exactly what remains, otherwise.\n\n\
                      It then finishes or undoes an operation that rewrites the whole staging \
                      area (shift, park, a merge, pick or fast-forward, lower, git import, \
                      bay creation) and was interrupted (\"incomplete_load\" refusals naming it, \
                      exit 20): rolled forward where the bay's state shows it got far enough, \
                      rolled back otherwise. Either way it rewrites only files that still hold \
                      one side's content or are missing; any other file (an edit made since, or \
                      a merge's combination of both sides) is left and reported as a local \
                      change, never staged."
    )]
    Heal,

    /// Print the command list, or detailed help about one command
    Help {
        /// The command to explain (subcommands work too, e.g. "forklift help office admit")
        #[arg(value_name = "COMMAND")]
        command: Vec<String>,
    },

    /// Print the parcels of a revision, newest first
    #[command(
        visible_alias = "hi",
        alias = "log",
        long_about = "Print the parcels of the current pallet (or of the given revision — a pallet \
                      name or a parcel hash), newest first: hash, operators, timestamps and \
                      description. Read a long history in pages: --limit caps a page, and \
                      --after resumes from the cursor the previous page returned. To filter on \
                      identity, signer or provenance, use \"forklift query\"."
    )]
    History {
        /// A pallet name or a parcel hash prefix (default: the current pallet)
        revision: Option<String>,

        /// Show at most this many parcels (newest first) — a bounded walk that never loads
        /// the whole history
        #[arg(long, short = 'n', value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..))]
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

    /// Upload the current pallet's new parcels to the remote (push)
    #[command(
        visible_alias = "li",
        alias = "push",
        long_about = "Upload the current pallet's new parcels to the configured remote \
                      (\"remote.url\") and move the remote's ref. When trust is established, the \
                      office pallet and the trust anchor are lifted first — the remote verifies \
                      every signature before it accepts the update. A remote that is ahead or has \
                      diverged is never overwritten: lower first when it is ahead; when both \
                      sides moved and the changes do not auto-merge, the refusal names how to \
                      merge the remote head in. A pallet deleted on the remote is re-created only \
                      with \"--recreate\", from work that descends from the head it was deleted at."
    )]
    Lift {
        /// Re-create the pallet on the remote where it was deleted, acknowledging the deletion:
        /// refused unless the local work descends from the head the remote's tombstone holds
        #[arg(long)]
        recreate: bool,
    },

    /// Take the current state of a file or directory into the inventory (stage)
    #[command(visible_alias = "l", alias = "add")]
    Load {
        /// The file or directory to load
        path: String,
    },

    /// Download the remote's new parcels and fast-forward the current pallet, or another (pull)
    #[command(
        visible_alias = "lo",
        alias = "pull",
        long_about = "Download the remote's new parcels for the current pallet from the configured \
                      remote (\"remote.url\") and fast-forward to them — working directory and \
                      inventory included. Naming another pallet downloads the remote's history of \
                      that pallet and creates or fast-forwards its ref alone: this bay, its files \
                      and its inventory stay where they are, and \"forklift shift\" moves onto it. On first \
                      contact with a trusted remote its trust anchor is adopted (one-way, like \
                      \"forklift office enroll\"), and the adopted genesis is reported. A diverged pallet is \
                      reported, never merged implicitly. A diverged metadata pallet (@kv, @tags, \
                      @manifest, @haul) replays the local changes onto the remote's; paths both \
                      sides changed refuse the lower with a meta_conflict error (exit 24) naming \
                      every pallet, path and both sides, until --keep resolves each one."
    )]
    Lower {
        /// The pallet to lower (default: the current pallet); "forklift pallet list --remote" lists
        /// the remote's
        pallet: Option<String>,

        /// Refuse unless the remote's trust anchor is this genesis (an out-of-band pin)
        #[arg(
            long = "expect",
            value_name = "GENESIS",
            long_help = "Refuse unless the remote's trust anchor is exactly this genesis, a value \
                         obtained out of band (from an operator you know, or a first-contact \
                         warning you verified). Checked right after the handshake, before anything \
                         is downloaded or created; a mismatch refuses with a trust_changed error. \
                         Without it, a first contact adopts whatever anchor the remote holds, and \
                         says which."
        )]
        expect: Option<String>,

        /// Resolve a metadata conflict: keep ours or theirs at a path (repeatable)
        #[arg(
            long = "keep",
            value_name = "@PALLET:PATH=SIDE",
            long_help = "Resolve a metadata conflict that a previous lower named. @PALLET is the \
                         metadata pallet with its @ (@kv, @tags, @manifest or @haul), PATH the \
                         path the conflict named: on @kv a subject (a full parcel hash) and, \
                         optionally, the key, as in @kv:<subject>/<key>; on @tags a tag name; on \
                         @manifest a subject and on @haul a full haul id, each optionally with \
                         the path beneath it. SIDE is ours or theirs. \"theirs\" drops every local change at or beneath the path; \
                         \"ours\" puts the local value back on top of the remote one, and must \
                         name a conflicted key exactly. \"theirs\" also discards a local change \
                         nobody else made, so a \"theirs\" resolution needs --confirm: without it, \
                         the lower lists what each one drops, changes nothing and exits non-zero. \
                         A resolution naming a path with nothing to resolve is refused. Repeat \
                         the flag for each path."
        )]
        keep: Vec<String>,

        /// Drop the local changes a "theirs" resolution names (only a "theirs" --keep needs it)
        #[arg(long)]
        confirm: bool,
    },

    /// Facts recorded about a parcel after the fact: notes, approvals, provenance, keyed values
    #[command(
        long_about = "Record and read the manifest — signed facts attached to a parcel after the \
                      fact. Notes, approvals and machine-authorship provenance are entries on the \
                      \"@manifest\" meta pallet (subcommands note, approve, provenance and \
                      show). Keyed values, such as a build status or a ticket link, live on the \
                      \"@kv\" meta pallet (subcommands set, get, unset, keys and log): a key \
                      holds one text value per parcel, and lower combines changes to different \
                      keys, while a key changed differently on both sides is a conflict that \
                      \"forklift lower --keep\" resolves. Every change is signed by its writer, \
                      verifiable offline against the office chain, and references the parcel \
                      without ever changing it. Recording needs an enrolled signing key; a keyed \
                      value also needs a writer or admin role. Without a subcommand, every \
                      \"@manifest\" entry is listed."
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
        long_about = "Manage the warehouse office: users and signing keys, tracked as metadata on \
                      the \"@office\" meta pallet. Without a subcommand, the users and their \
                      keys are listed."
    )]
    Office {
        #[command(subcommand)]
        action: Option<OfficeAction>,
    },

    /// Manage pallets (branches): list, create, delete and restore them
    #[command(
        alias = "branch",
        long_about = "Manage pallets, Forklift's branches. Without a subcommand, the pallets are \
                      listed, as \"forklift pallet list\" does."
    )]
    Pallet {
        #[command(subcommand)]
        action: Option<PalletAction>,
    },

    /// Park the work in progress and reset to the pallet head
    #[command(
        visible_alias = "pa",
        alias = "stash",
        long_about = "Park the work in progress (the staged and unstaged changes of tracked files) \
                      as a parked parcel and reset the warehouse to the pallet head. Untracked \
                      files are left alone. \"forklift park pop\" re-applies the latest parked changes; \
                      \"forklift park list\" lists them."
    )]
    Park {
        #[command(subcommand)]
        action: Option<ParkAction>,
    },

    /// Apply a parcel's changes onto the current pallet as a new, author-preserving parcel
    #[command(
        alias = "cherry-pick",
        long_about = "Apply a parcel's diff (its change against its first parent) onto the current \
                      pallet as a new, author-preserving, freshly-signed parcel. Unlike \
                      rebase, pick only *adds* — no rewrite, no force-push: the picked \
                      parcel's authors are preserved and this operator is recorded as the stacker. \
                      A clean pick is stacked immediately; a conflicting one leaves markers to \
                      resolve, \"forklift load\" and \"forklift stack\" (which completes the pick single-parent)."
    )]
    Pick {
        /// The parcel to pick: a pallet name, or a parcel hash / unique prefix
        revision: String,

        /// The new parcel's message (default: the source parcel's message)
        #[arg(short = 'm', long)]
        message: Option<String>,
    },

    /// Prepare a new warehouse in the current directory
    #[command(
        alias = "init",
        long_about = "Prepare a new warehouse in the current directory: the \".forklift\" folder \
                      with the object store, the staging area and the configuration template. No \
                      ignore file is written: \".forkliftignore\" is tracked content, created when \
                      you need one. Preparing is idempotent — only the missing pieces are created."
    )]
    Prepare,

    /// Manage identity profiles (named operator ids) and select one per warehouse
    #[command(
        long_about = "Manage identity profiles: named operator ids stored in the global \
                      configuration. A warehouse selects one \
                      with \"forklift profile use\", so one machine can act as different operators in \
                      different warehouses — e.g. a personal identity and one per organization. \
                      \"forklift profile keygen\" generates a signing key for one. Without a \
                      subcommand, the profiles are listed."
    )]
    Profile {
        #[command(subcommand)]
        action: Option<ProfileAction>,
    },

    /// Query parcel history by signed identity, signer and parcel facts
    #[command(
        long_about = "Filter parcel history on its signed dimensions — identity class, \
                      supervisor, role, signing key, recorded machine-authorship provenance, \
                      signed tags — plus parcel-local facts (dates, description, merge-ness, \
                      touched paths). By default identity answers are VERIFIED: the \
                      signature is checked and the office joined off the verified signer, never \
                      off the parcel's own (forgeable) claim; --recorded opts into the cheap, \
                      self-declared reading and labels every answer accordingly. A parcel with \
                      no provenance recorded at all never matches a --model/--tool predicate in \
                      either direction (absence isn't a claim to negate); an untagged parcel \
                      plainly does not carry a --tag. Flags AND together; --where takes a JSON \
                      predicate tree for or/not/nesting. Scope the walk with revisions and \
                      --from — that (not --limit) is what bounds verification work. A match \
                      signed by a since-revoked key carries signer.boundary: vouched (inside the \
                      history its revocation vouched for), suspect (outside it: a forged \
                      backdate, or the key's holder kept signing after the revocation), or \
                      unresolved (a partial clone lacks a boundary head or an ancestor of one, so \
                      the question cannot be answered here; never read it as suspect). A \
                      --where leaf on signer.boundary filters on vouched or suspect."
    )]
    Query {
        /// Walk from these revisions (pallet names or parcel hashes; default: the current
        /// pallet's head)
        #[arg(value_name = "REVISION")]
        revisions: Vec<String>,

        /// Exclude this revision and all its ancestors from the walk (git's A..B shape)
        #[arg(long, value_name = "REVISION")]
        from: Option<String>,

        /// Verified author identity class: human | agent | bot | service
        #[arg(long)]
        class: Option<String>,

        /// Verified author is automated (agent/bot/service) and has no supervisor
        #[arg(long)]
        unsupervised: bool,

        /// Verified author is supervised by this operator id
        #[arg(long, value_name = "OPERATOR")]
        supervisor: Option<String>,

        /// Verified signing key (id prefix)
        #[arg(long, value_name = "KEYPREFIX")]
        signer: Option<String>,

        /// Only parcels authored after this RFC 3339 timestamp
        #[arg(long, value_name = "TIMESTAMP")]
        author_after: Option<String>,

        /// Only parcels authored before this RFC 3339 timestamp
        #[arg(long, value_name = "TIMESTAMP")]
        author_before: Option<String>,

        /// Only merge parcels (more than one parent)
        #[arg(long, conflicts_with = "no_merges")]
        merges: bool,

        /// Only non-merge parcels
        #[arg(long)]
        no_merges: bool,

        /// Description matches this glob (*, ?) or contains it as a substring
        #[arg(long, value_name = "GLOB")]
        grep: Option<String>,

        /// Recorded machine-authorship model matches this glob (*, ?) or contains it as a
        /// substring (from a manifest provenance entry; absence of any entry never matches)
        #[arg(long, value_name = "GLOB")]
        model: Option<String>,

        /// Recorded machine-authorship tool matches this glob (*, ?) or contains it as a
        /// substring (from a manifest provenance entry; absence of any entry never matches)
        #[arg(long, value_name = "GLOB")]
        tool: Option<String>,

        /// Carries this signed tag (from the @tags meta pallet)
        #[arg(long, value_name = "NAME")]
        tag: Option<String>,

        /// Changed this warehouse path (or a path under it) versus the parcel's first
        /// parent only — a merge whose result at the path already matches its first parent
        /// (e.g. both sides added identical content) does not match, even if the other
        /// parent touched it (see "forklift help query")
        #[arg(long, value_name = "PATH")]
        touches: Option<String>,

        /// Verify identity answers (the default; the flag exists to say so explicitly)
        #[arg(long, conflicts_with = "recorded")]
        verify: bool,

        /// Use the recorded (self-declared, forgeable) identity instead of verifying:
        /// cheaper, prunes early, and every answer is labeled "recorded"
        #[arg(long)]
        recorded: bool,

        /// A JSON predicate tree ("-" reads it from stdin) AND-ed with the flags
        #[arg(
            long,
            value_name = "JSON",
            long_help = "A JSON predicate tree AND-ed with the flags (\"-\" reads it from stdin). \
                         A node is {\"all\": [nodes]}, {\"any\": [nodes]}, {\"not\": node} or a \
                         leaf {\"field\", \"op\", \"value\"}; an unknown field is refused with \
                         the list of fields."
        )]
        r#where: Option<String>,

        /// Show at most this many matches (bounds the output page, never the verification
        /// work — scope the walk with revisions or --from for that)
        #[arg(long, short = 'n', value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..))]
        limit: Option<usize>,

        /// Resume paging from a cursor returned as `next` by a previous `--json` page
        #[arg(long, value_name = "CURSOR")]
        after: Option<String>,

        /// One line per match: the abbreviated hash and the description's first line
        #[arg(long)]
        oneline: bool,
    },

    /// Stage a file or directory for removal
    #[command(
        visible_alias = "rm",
        long_about = "Stage a file or directory for removal: its inventory entries are marked as \
                      deleted, so they will not be part of the next parcel. The working directory \
                      is not touched. To unstage changes instead (undo a \"forklift load\"), use \
                      \"forklift unload\"."
    )]
    Remove {
        /// The file or directory to stage for removal
        path: String,
    },

    /// Restore a file or directory from the inventory (discard unstaged changes)
    #[command(
        visible_alias = "r",
        long_about = "Restore a file or directory in the working directory from the inventory \
                      (discard unstaged changes). When that would overwrite a changed file, it \
                      lists the files whose changes would be lost, changes nothing and exits \
                      non-zero unless --confirm is given; restoring a missing file needs no \
                      --confirm. To unstage instead, leaving the working directory untouched, use \
                      \"forklift unload\"."
    )]
    Restore {
        /// The file or directory to restore
        path: String,

        /// Discard the unstaged changes the restore would overwrite
        #[arg(long)]
        confirm: bool,
    },

    /// Manage this binary itself: its update, its short alias and its self-description
    #[command(
        name = "self",
        subcommand_required = true,
        arg_required_else_help = true,
        long_about = "Manage this forklift binary itself rather than a warehouse. \"forklift self \
                      update\" checks for a newer release and installs it, \"forklift self alias\" \
                      manages the `fl` short name next to the binary, and \"forklift self schema\" \
                      describes the command surface for Pult."
    )]
    SelfCommand {
        #[command(subcommand)]
        action: SelfAction,
    },

    /// Shift to another pallet
    #[command(
        visible_alias = "sh",
        aliases = ["checkout", "switch"],
        long_about = "Shift to another pallet: materialize its head parcel in the working \
                      directory and repopulate the inventory from it. Refuses to run while there \
                      are staged or unstaged changes (untracked files are tolerated). Shifting to \
                      the current pallet catches up with it when another bay or process moved it: \
                      a clean bay's files move to its head."
    )]
    Shift {
        /// The pallet to shift to
        pallet: String,
    },

    /// Print a file's content at a revision, as git show does for REV:PATH
    #[command(
        long_about = "Print a file's content at a revision in one invocation: \
                      \"<revision>:<path>\", split on the first \":\" (a revision — a pallet \
                      name, an \"@\"-qualified meta pallet, or a parcel hash prefix — can never \
                      contain \":\", so the split is unambiguous even when the path itself has \
                      one). A chunked large file reports its metadata (content hash, size, chunk \
                      count) instead of assembling it; non-text content is reported binary. With \
                      --json, the envelope carries the resolved parcel hash, the tree entry's own \
                      hash, the binary/chunked signal, and the content when it is text."
    )]
    Show {
        /// "<revision>:<path>"
        #[arg(value_name = "REVISION:PATH")]
        target: String,
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
        /// The parcel's description
        #[arg(short = 'm', long)]
        message: Option<String>,
    },

    /// Report the staged and unstaged changes of the warehouse
    #[command(
        visible_alias = "st",
        alias = "status",
        long_about = "Report the current pallet, the staged changes (inventory vs pallet head — \
                      what the next \"forklift stack\" records) and the changes not yet loaded (working \
                      directory vs inventory). With --summary, report only the counts (a \
                      token-cheap overview for scripts and agents)."
    )]
    Stocktake {
        /// Report only the counts, not the per-path changes
        #[arg(long)]
        summary: bool,
    },

    /// Show and change the sparse scopes: this checkout's scope and the warehouse fetch scope
    #[command(
        long_about = "Show and change the two sparse scopes. The checkout scope is what one \
                      checkout (a bay or the main tree) writes to disk; the fetch scope is what \
                      the warehouse's store holds, shared by every checkout. Without a \
                      subcommand, show both, as \"forklift scope status\" does. \"forklift scope \
                      narrow\" shrinks the checkout scope; \"forklift scope fetch\" widens the \
                      fetch scope and \"forklift scope prune\" shrinks it."
    )]
    Scope {
        #[command(subcommand)]
        action: Option<ScopeAction>,
    },

    /// Show the remote and the origin, or move a partial warehouse's origin to another server
    #[command(
        long_about = "Show where this warehouse lifts to and lowers from: the configured remote \
                      (\"remote.url\") and, in a partial warehouse (one cloned with \"--only\" \
                      or \"--cache\"), its origin, with any move of the origin that has not \
                      finished. A partial warehouse talks only to its origin, the remote it was \
                      cloned from, because only the origin was proved to hold the content it \
                      lacks; \"forklift remote move\" is the one way to change it. A full \
                      warehouse has no origin and may talk to any remote."
    )]
    Remote {
        #[command(subcommand)]
        action: Option<RemoteAction>,
    },

    /// Inspect and maintain the object store: its health, packing, raw records and cached content
    #[command(
        long_about = "Inspect and maintain the object store. Without a subcommand, report its \
                      health, as \"forklift store status\" does."
    )]
    Store {
        #[command(subcommand)]
        action: Option<StoreAction>,
    },

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

    /// Undo the last stack, consolidate, pick, shift or pallet delete, keeping your changes
    #[command(
        long_about = "Undo the last journaled operation: a \"forklift stack\", \"forklift consolidate\", \
                      \"forklift pick\", \"forklift shift\" or \"forklift pallet delete\". A stack, \
                      consolidate or pick is reversed softly: the pallet head moves back while the working directory and \
                      the inventory stay as they are (like git's \"reset --soft HEAD~1\"), so the \
                      undone changes are staged again and can be re-stacked — e.g. with a \
                      corrected message or author. A shift is reversed by shifting back. Undo \
                      reverses only the user's last operation: except for a pallet delete, which \
                      changed nothing in the bay, if the current pallet has been \
                      switched (e.g. by pallet create or deliver), its head has moved, or a \
                      consolidation or pick was started or finished since the journaled \
                      operation, it refuses \
                      instead of reaching past that. In a warehouse that has been served, a \
                      stack, consolidate or pick is not undone when it would move a user \
                      pallet back off the head it left, since the warehouse's clones may rely on \
                      that head: stack a parcel that reverts the change instead. An unreadable \
                      journal is reported by name, never read as empty. A pallet delete is \
                      reversed by bringing the pallet back at the head it was deleted at; if that \
                      name has been taken since, \"--as <new-name>\" brings it back under another \
                      name."
    )]
    Undo {
        /// When the last journaled operation is a pallet delete: bring the deleted pallet back
        /// under this name instead of its own. The bay stays where it is
        #[arg(long = "as", value_name = "NEW_NAME")]
        as_name: Option<String>,
    },

    /// Unstage a file or directory (keep your changes)
    #[command(
        visible_alias = "ul",
        long_about = "Unstage a file or directory: reset its inventory entries to the pallet \
                      head, leaving the working directory untouched — the inverse of \"forklift load\". \
                      To stage a removal instead, use \"forklift remove\"."
    )]
    Unload {
        /// The file or directory to unstage
        path: String,
    },

    /// Print the version of Forklift
    Version,

    /// [internal] Print a generated documentation fragment (`bin/gen-docs` only)
    ///
    /// Not part of the public CLI surface: only compiled in with the `docgen` feature, which
    /// a release build never enables, and hidden from help either way. `bin/gen-docs` is the
    /// only intended caller.
    #[cfg(feature = "docgen")]
    #[command(name = "__docgen", hide = true)]
    Docgen {
        #[arg(value_enum)]
        target: DocgenTarget,
    },
}

/// Which generated documentation fragment `forklift __docgen` prints to stdout.
#[cfg(feature = "docgen")]
#[derive(Clone, clap::ValueEnum)]
pub enum DocgenTarget {
    /// The error-code / exit-code reference table (`docs/generated/errors.md`).
    Errors,
    /// Every command's `--json` `data` payload schema (`docs/generated/json-schemas.md`).
    JsonSchemas,
    /// Every command and subcommand with its aliases (`docs/generated/commands.md`).
    Commands,
    /// Every MCP tool with its command and arguments, and the commands no agent drives
    /// (`docs/generated/mcp-tools.md`).
    McpTools,
}

#[derive(Subcommand)]
pub enum SelfAction {
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

    /// Describe this binary's command surface as a Pult manifest
    #[command(
        long_about = "Print Forklift's command surface in Pult's manifest vocabulary, generated \
                      from this binary's own argument definitions: one command per runnable \
                      command path, each with its params and the argument tail to run. Under \
                      --json the document is the envelope's data. It is input for `pult tool`, \
                      which prefixes the binary's path; it is not a runnable pult.yaml on its \
                      own, because its runs omit the program name."
    )]
    Schema,

    /// Check for a newer release and update this binary (client only)
    #[command(
        long_about = "Check the latest published release against this binary and, for a \
                      script/manual install, update in place by re-running the verified install \
                      script. A package-manager install (cargo/brew) is never overwritten — \
                      self update shows the right upgrade command instead. There is deliberately \
                      no server self update: a server is redeployed, not self-mutated."
    )]
    Update {
        /// Only report whether an update is available and how to get it; change nothing
        #[arg(long)]
        check: bool,
    },
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
                      inventory and current pallet. cd into it to work there. A scoped bay \
                      (--scope) is how an orchestrator hands a sub-agent a task-scoped sandbox."
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
                         warehouse fetched (see \"forklift clone --only\" and \"forklift scope fetch\"). Repeatable to \
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
    /// Show this checkout's scope and the warehouse fetch scope (the default)
    #[command(
        long_about = "Show the two sparse scopes: this checkout's scope (the subtree path(s) the \
                      bay or the main tree checks out, stages and stacks) and the warehouse's \
                      fetch scope (what the store has fetched at all). An unscoped bay (or the \
                      main tree) reports a full checkout scope: the whole tree. A checkout scope \
                      is local to the checkout and never tracked. Read-only."
    )]
    Status,
    /// Widen a sparse warehouse's fetch scope and download the newly in-scope subtree(s)
    #[command(
        long_about = "Widen a sparse warehouse's fetch scope: add subtree path(s) to what the \
                      store fetches, then download their content across the whole history from the \
                      remote. Cheap and incremental — only the newly in-scope objects are fetched; \
                      what is already present is skipped. Every checkout's scope stays as it is: \
                      after fetching, a bay can be scoped to the new path (\"forklift bay add \
                      --scope\"). A full (non-sparse) warehouse already holds everything, so there \
                      is nothing to fetch."
    )]
    Fetch {
        /// The subtree path(s) to add to the fetch scope. Repeatable.
        #[arg(value_name = "PATH", required = true)]
        paths: Vec<String>,
    },
    /// Narrow this checkout's scope and remove the dropped subtree(s) from the working directory
    #[command(
        long_about = "Narrow this checkout's scope: drop subtree path(s) from what this bay (or \
                      the main tree) checks out, and remove the now out-of-scope files from the \
                      working directory. This frees nothing in the shared object store — the \
                      dropped content is still ordinary reachable history, not garbage — it only \
                      shrinks what this checkout shows, and the fetch scope stays as it is. A \
                      checkout must keep at least one in-scope path; to stop scoping entirely, \
                      open a fresh full checkout."
    )]
    Narrow {
        /// The in-scope subtree path(s) to drop. Repeatable.
        #[arg(value_name = "PATH", required = true)]
        paths: Vec<String>,
    },
    /// Reclaim disk from a sparse warehouse: shrink its fetch scope and free the content
    #[command(
        long_about = "Reclaim disk from a sparse warehouse: drop a fetched path from the shared \
                      warehouse fetch scope and free the objects under it from the object store. \
                      This is the deliberate, destructive counterpart of \"forklift scope narrow\": \
                      narrow shrinks one checkout's scope and frees nothing, because narrowed-away \
                      content is still reachable history that garbage collection correctly keeps; \
                      prune shrinks the fetch scope warehouse-wide and reclaims its disk. It is \
                      multi-bay-aware: it refuses to free a path any checkout (the main tree or a \
                      bay) still has in its scope. It frees only history the remote holds, so it \
                      asks the remote first and refuses, changing nothing, when it cannot; work \
                      never lifted keeps its content until it is lifted and the prune is run \
                      again. What it frees is sealed by hash afterwards — fetched again from the \
                      origin with \"forklift scope fetch\". A full (non-sparse) warehouse has \
                      nothing to prune. If an earlier prune was interrupted before it finished \
                      freeing everything, running it again on the same path resumes and finishes \
                      the job rather than refusing. Without --confirm it shows what it would free, \
                      changes nothing and exits non-zero; when there is nothing to free, it runs \
                      without --confirm. A confirmed run plans again, so if the remote moved meanwhile it \
                      may free a little more or less than the preview showed."
    )]
    Prune {
        /// The fetched path(s) to prune. Repeatable; also accepts a path an earlier, interrupted
        /// prune already dropped from the fetch scope, to resume and finish freeing it.
        #[arg(value_name = "PATH", required = true)]
        paths: Vec<String>,

        /// Free the content (without it, what would be freed is shown and nothing changes)
        #[arg(long)]
        confirm: bool,
    },
}

#[derive(Subcommand)]
pub enum PalletAction {
    /// List the pallets, marking the current one (the default)
    #[command(
        long_about = "List the pallets, marking the current one. With \"--remote\", list the \
                      remote's pallets instead, each with how it relates to the local one, and the \
                      pallets deleted there."
    )]
    List {
        /// Also show the meta pallets (the office and other tracked metadata; reach them with the
        /// "@" qualifier, e.g. "forklift history @office")
        #[arg(short, long)]
        all: bool,

        /// List the remote's pallets instead: each with its remote head, the local head, and
        /// whether the two are the same, the local one is ahead, or they differ (lower decides),
        /// and the pallets deleted there. Downloads and writes nothing
        #[arg(long)]
        remote: bool,
    },
    /// Create a new pallet and shift to it
    #[command(
        long_about = "Create a new pallet at the current head, or at the given revision (a pallet \
                      name or a parcel hash), and shift to it."
    )]
    Create {
        /// The name of the new pallet
        name: String,

        /// Where the new pallet starts: a pallet name or a parcel hash prefix
        /// (default: the current pallet's head)
        revision: Option<String>,
    },
    /// Delete a pallet: remove its ref, keeping its parcels
    #[command(
        long_about = "Delete a pallet: its ref goes, its parcels stay, and \"forklift undo\" brings \
                      it back. Refused for the pallet a bay stands on, one an open haul names, and, \
                      without \"--force\", one whose work no other pallet here or on the remote \
                      holds. With \"--remote\", delete the remote's pallet instead: it keeps its \
                      history on the remote, and no clone's own pallet changes."
    )]
    Delete {
        /// The pallet to delete
        name: String,

        /// Delete even when no other pallet holds the pallet's work. It stays recoverable by
        /// "forklift undo" while the delete is in this bay's undo journal, and in a warehouse a
        /// server has served by "forklift pallet restore" while its tombstone stands
        #[arg(long, conflicts_with = "remote")]
        force: bool,

        /// Delete the remote's pallet instead of the local one
        #[arg(long)]
        remote: bool,
    },
    /// Bring back a deleted pallet at the head its tombstone holds
    #[command(
        long_about = "Bring back a pallet deleted in this warehouse after it was served, at the \
                      head its tombstone holds. Refused while a bay stands on the name at another \
                      head. With \"--remote\", bring it back on the remote, which needs no history \
                      here."
    )]
    Restore {
        /// The pallet to bring back
        name: String,

        /// Bring the remote's pallet back instead of the local one
        #[arg(long)]
        remote: bool,
    },
}

#[derive(Subcommand)]
pub enum StoreAction {
    /// Report object-store health: loose vs packed objects, packs, and whether compaction is due
    /// (the default)
    #[command(
        long_about = "Report the health of the object store — the counterpart of \"forklift stocktake\" \
                      (which reports the working tree). Shows how many objects are loose \
                      (unpacked) versus packed, how many pack files there are and how \
                      delta-dense they are, the on-disk sizes, and whether an incremental \
                      compaction or a consolidating repack is currently due per the \
                      maintenance.loose / maintenance.packs thresholds. Read-only; run \
                      \"forklift store compact\" to act on it."
    )]
    Status,
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
                      Run it after a large \"forklift git import\" or periodically as maintenance. With \
                      --all, also rewrite the existing packs: drop unreachable (garbage) \
                      objects that were stuck in packs and consolidate many packs into few — a \
                      heavier full repack, worth running occasionally. Add --redelta to a full \
                      repack to also re-delta-compress every object instead of copying already-\
                      packed records verbatim — denser, but a one-shot CPU-bound pass."
    )]
    Compact {
        /// Repack existing packs too: drop unreachable objects and consolidate (a full repack)
        #[arg(long)]
        all: bool,

        /// Re-encode every object to delta-compress across the whole store (only with --all)
        #[arg(
            long,
            long_help = "Re-encode every live object — packed or loose, files and directories \
                         alike — to delta-compress across the whole store, instead of a \
                         repack's usual verbatim copy of already-packed records. Buys back the \
                         cross-path similarity (renames, moved files) a per-object copy cannot \
                         see, at the cost of a full re-read, re-compress and re-delta of every \
                         object: one-shot and CPU-bound, not something to run routinely. Safe to \
                         repeat any number of times on the same store. Only valid with --all."
        )]
        redelta: bool,
    },
    /// Peek into an object (print its content)
    #[command(
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
    /// Free cached content the origin holds, to be fetched again when read
    #[command(
        long_about = "Free disk in a cache warehouse (one cloned with \"--cache\"): delete the \
                      content its origin holds and nothing here needs, so that a later read fetches \
                      it again. Parcels and signatures always stay. It asks the origin which history \
                      it holds first, and refuses, changing nothing, when the configured remote is \
                      not the origin or the origin cannot be reached; work never lifted keeps its \
                      content. It keeps every checkout's files, metadata and trust records, staged \
                      work, and anything a parked or in-progress operation needs. If it is \
                      interrupted, running it again finishes the job. Objects inside packs are not \
                      freed yet. Use --dry-run to see what it would free without changing anything."
    )]
    Evict {
        /// Report what would be freed and change nothing
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
pub enum GitAction {
    /// Import a git repository's history into this warehouse (one-way migration)
    #[command(
        long_about = "One-way migration of a git repository's history into this warehouse: \
                      git commits become parcels, trees become trees, blobs become blobs, and each \
                      local branch becomes a pallet. The imported history is unsigned (it predates \
                      trust), so import into a fresh warehouse and then run \"forklift office enroll\" to \
                      anchor it. Run it from the git repo's directory (\"forklift git import .\") so \
                      the checked-out working tree already holds the imported HEAD. Forklift \
                      converts nothing on checkout, so a file git converted (line endings, ident, \
                      a filter) reads as modified, and the import warns about it. Requires git \
                      on PATH."
    )]
    Import {
        /// The git repository to import from (e.g. \".\")
        path: String,

        /// Store the imported objects loose instead of packing (and delta-compressing) them
        /// on the way in
        #[arg(long)]
        no_compact: bool,
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
    Export {
        /// Where to create the git repository (must be empty or new)
        path: String,
    },
    /// Mirror the remote's pallets to a git remote (mirror.url), never by force
    #[command(
        long_about = "Keep the git remote at mirror.url in step with the pallets the Forklift \
                      remote holds (mirror.pallets, or every user pallet). Each pallet's remote head \
                      is fetched, exported incrementally into .forklift/mirror.git, and pushed to the \
                      git branch of the same name, never by force: a git branch holding a commit the \
                      export does not is refused, and left as it is. Only lifted heads are mirrored. \
                      Tags, the signed office, the @manifest and signatures are not mirrored. A \
                      head is exported only once its history verifies against the trust anchor \
                      (the one held, or the one the remote serves). With \
                      mirror.auto on (the default), every lift mirrors the pallet it lifted. \
                      Requires git on PATH."
    )]
    Mirror,
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
                      with an agent-class identity (\"forklift office admit --agent --supervisor\"), it \
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

    /// Set a keyed value on a parcel
    #[command(
        long_about = "Set a key on a parcel to a text value, replacing any value it held. A key \
                      is ASCII letters, digits, \".\", \"_\" and \"-\", at most 128 bytes, and \
                      never begins with \".\" or \"-\"; a value is at most 64 KiB of text. \
                      Requires an enrolled signing key and a writer or admin role."
    )]
    Set {
        /// The parcel the value is about (a pallet name, or a parcel hash / unique prefix)
        revision: String,

        /// The key
        key: String,

        /// The value
        value: String,
    },

    /// Show a keyed value on a parcel
    Get {
        /// The parcel (a pallet name, or a parcel hash / unique prefix)
        revision: String,

        /// The key
        key: String,
    },

    /// Remove a keyed value from a parcel
    #[command(
        long_about = "Remove a key from a parcel. A key that holds no value is refused. Requires \
                      an enrolled signing key and a writer or admin role."
    )]
    Unset {
        /// The parcel (a pallet name, or a parcel hash / unique prefix)
        revision: String,

        /// The key
        key: String,
    },

    /// List every keyed value, or a parcel's
    Keys {
        /// Only this parcel's values (a pallet name, or a parcel hash / unique prefix)
        revision: Option<String>,
    },

    /// Show every change to a key, newest first, with who made it
    #[command(
        long_about = "Show every change to a key on a parcel, newest first: the parcel that made \
                      it, when, by whom, and the value after it. Walks the whole history of the \
                      @kv meta pallet."
    )]
    Log {
        /// The parcel (a pallet name, or a parcel hash / unique prefix)
        revision: String,

        /// The key
        key: String,
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

    /// Propose the source pallet's current head
    #[command(
        long_about = "Move an open haul to the source pallet's current head. Only the haul's opener \
                      moves it; a rewound or rebased source moves the haul with it. Reviews record \
                      the head they reviewed, so a review of an earlier head stays listed but no \
                      longer counts as an approval of the haul."
    )]
    Update {
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

    /// Record a signed review: approve, request changes or comment
    #[command(
        group = clap::ArgGroup::new("verdict").required(true).args(["approve", "request_changes", "comment"]),
        long_about = "Record a signed review of a haul's current head. The verdict is always \
                      stated: exactly one of --approve, --request-changes or --comment, so a review \
                      never records an approval its reviewer did not give. Because the review is \
                      signed, the approval carries your identity class — an agent's approval is \
                      distinguishable from a human's. Requires an enrolled signing key."
    )]
    Review {
        /// The haul id (or a unique prefix)
        id: String,

        /// Approve the haul's current head
        #[arg(long)]
        approve: bool,

        /// Request changes
        #[arg(long)]
        request_changes: bool,

        /// A plain review comment (no approval)
        #[arg(long)]
        comment: bool,

        /// The review message
        #[arg(short = 'm', long)]
        message: Option<String>,
    },

    /// Land a proposal: consolidate its source into the target
    #[command(alias = "merge")]
    Consolidate {
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
pub enum RemoteAction {
    /// Move a partial warehouse's origin to another server that holds its content
    #[command(
        long_about = "Point a partial warehouse at a new origin. The move first records that it is \
                      under way, so nothing fetches, lifts or changes what it checks until it ends. \
                      It then asks the new server for its pallets, judges its trust anchor as \
                      \"forklift lower\" would, fetches the parcels and signatures it lacks (no content), \
                      and checks that the new server holds everything this warehouse leans on its \
                      origin for: every tree, file and chunk that something here references and \
                      this warehouse does not hold. Only then does it rewrite the origin and \"remote.url\" (and \
                      \"remote.token\" with --token). If the check fails, nothing is rewritten and \
                      the origin stays; when the new server lacks content, copy the old server's \
                      warehouse to it server-side and run the move again. A move that stopped \
                      before it started rewriting can be abandoned with \"forklift remote abort\"; \
                      one that stopped while rewriting is finished by running the move again. A full warehouse has \
                      no origin: set \"remote.url\" with \"forklift config set\" instead."
    )]
    Move {
        /// The new origin's URL
        url: String,

        /// The bearer token for the new origin, stored as remote.token when the move completes
        /// (default: the current remote.token)
        #[arg(long)]
        token: Option<String>,
    },

    /// Abandon a move that stopped before it started rewriting the origin
    #[command(
        long_about = "Abandon a move of the origin that stopped before it started rewriting: the \
                      origin and \"remote.url\" stay as they were, and fetching and lifting work \
                      again. A move that stopped while rewriting cannot be abandoned, because part \
                      of this warehouse already points at the new server; run the move again to \
                      finish it. With no move under way, it reports that and changes nothing."
    )]
    Abort,
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

    /// Enroll another operator with their public key — admins only
    #[command(
        long_about = "Enroll another operator: their UUID, public key and proof-of-possession, \
                      exactly as printed by their \"forklift profile keygen\" run. The key becomes \
                      their identity root, endorsed by your admin key. Office records are \
                      pseudonymous — no names or emails ever go on-chain. Mark automated \
                      principals with --agent / --bot / --service: the class rides in the \
                      signed record, so \"an agent wrote this, supervised by <human>\" is \
                      forge-proof. An agent requires a --supervisor; a supervisor must be an \
                      enrolled human. Automated identities should hold passphraseless keys (they \
                      sign autonomously under their own marked identity, never as a human)."
    )]
    Admit {
        /// The operator's UUID (printed by their "forklift profile keygen")
        #[arg(value_name = "OPERATOR_UUID")]
        operator_id: String,

        /// The operator's public key (64 hex characters)
        #[arg(value_name = "PUBLIC_KEY_HEX")]
        public_key: String,

        /// The key's proof-of-possession (printed by their "forklift profile keygen")
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
                      \"forklift profile keygen\" on the other device (configured with the same \
                      operator.uuid) and pass the printed public key and proof-of-possession \
                      here."
    )]
    Link {
        /// The new key's public half (64 hex characters)
        #[arg(value_name = "PUBLIC_KEY_HEX")]
        public_key: String,

        /// The new key's proof-of-possession (printed by "forklift profile keygen")
        #[arg(value_name = "PROOF_OF_POSSESSION_HEX")]
        pop: String,
    },

    /// Authorize a new key for an already-enrolled operator — admins only
    #[command(
        long_about = "Authorize a new key for an already-enrolled operator: the recovery path for \
                      someone who lost every device (the scope of a key-authorization \
                      equals the scope of the authorizer's authority, so an admin's endorsement \
                      is valid exactly here, in the office they administer). The operator runs \
                      \"forklift profile keygen\" on their new machine and hands you the printed \
                      line over a channel where you can confirm it is really them; their \
                      proof-of-possession still co-signs the record, so you cannot attribute a \
                      key they do not hold. For your own devices use \"forklift office link\"; for a new \
                      operator use \"forklift office admit\"."
    )]
    Authorize {
        /// The enrolled operator's UUID (printed by their "forklift profile keygen")
        #[arg(value_name = "OPERATOR_UUID")]
        operator_id: String,

        /// The new key's public half (64 hex characters)
        #[arg(value_name = "PUBLIC_KEY_HEX")]
        public_key: String,

        /// The new key's proof-of-possession (printed by "forklift profile keygen")
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
        /// Reset trust (without it, what the reset would do is shown and nothing changes)
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
                      confirming. Without --confirm, shows what would be accepted and refuses. \
                      --confirm takes the new genesis being accepted, and accepts only if that is \
                      still the remote's genesis — so what is accepted is exactly what was shown."
    )]
    AcceptRegenesis {
        /// Accept the new anchor, whose genesis must be this value (without it, the reset is only described)
        #[arg(long, value_name = "GENESIS")]
        confirm: Option<String>,
    },

    /// List the users and their keys (the default)
    List,
}

#[derive(Subcommand)]
pub enum ConfigAction {
    /// List every known key and its value (the default)
    List {
        /// List the per-user configuration that applies to all warehouses
        #[arg(short, long)]
        global: bool,
    },

    /// Print the value of a key
    Get {
        /// Read the per-user configuration that applies to all warehouses
        #[arg(short, long)]
        global: bool,

        /// The configuration key (e.g. "remote.url")
        key: String,
    },

    /// Set a key to a value
    Set {
        /// Write the per-user configuration that applies to all warehouses
        #[arg(short, long)]
        global: bool,

        /// The configuration key (e.g. "remote.url")
        key: String,

        /// The value to set the key to
        value: String,
    },

    /// Remove a key (e.g. clearing a "remote.token")
    Unset {
        /// Remove the key from the per-user configuration that applies to all warehouses
        #[arg(short, long)]
        global: bool,

        /// The configuration key (e.g. "remote.token")
        key: String,
    },
}

#[derive(Subcommand)]
pub enum ProfileAction {
    /// List the profiles and the local keys each one holds (the default)
    List,

    /// Create a named profile (mints an operator id unless one is given)
    Create {
        /// The profile name (e.g. "work")
        name: String,

        /// The operator id (e.g. one issued by a hosting provider); minted when omitted
        #[arg(long = "id", value_name = "OPERATOR_ID")]
        identifier: Option<String>,
    },

    /// Act as this profile in the current warehouse
    Use {
        /// The profile name
        name: String,
    },

    /// Generate a keypair locally and print its public half + proof-of-possession
    #[command(
        long_about = "Generate a keypair locally and print its public half plus a \
                      proof-of-possession (a self-signature binding the key to an operator id). \
                      The private key stays on this machine. Hand the printed admit line to an \
                      office admin to be enrolled, or the printed link line to a machine that \
                      already holds one of your keys to add this as another device. With a \
                      profile name, the key belongs to that profile's operator id. Without one, \
                      it belongs to the identity this directory acts as: the profile the \
                      enclosing warehouse selects, else the globally selected profile, else the \
                      warehouse's operator.identifier, else the global one (the default \
                      identity). It says which one it used. Runs outside a warehouse \
                      too, and records nothing in one."
    )]
    Keygen {
        /// The profile whose operator id the key belongs to; omit it for the current identity
        name: Option<String>,

        /// Protect the new key with a passphrase (recommended for human operators)
        #[arg(long)]
        passphrase: bool,
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
    /// `config` and `profile` are missing deliberately: they enter the warehouse themselves, and
    /// only when the warehouse is actually involved (global reads and writes, creating a profile
    /// and generating a key must work outside a warehouse).
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
                | Command::Pick { .. }
                | Command::Conflicts
                | Command::Consolidate { .. }
                | Command::Deliver { .. }
                | Command::Diff { .. }
                | Command::Git { .. }
                | Command::Heal
                | Command::History { .. }
                | Command::Lift { .. }
                | Command::Load { .. }
                | Command::Lower { .. }
                | Command::Haul { .. }
                | Command::Manifest { .. }
                | Command::Office { .. }
                | Command::Pallet { .. }
                | Command::Park { .. }
                | Command::Query { .. }
                | Command::Remove { .. }
                | Command::Remote { .. }
                | Command::Restore { .. }
                | Command::Scope { .. }
                | Command::Shift { .. }
                | Command::Show { .. }
                | Command::Stack { .. }
                | Command::Stocktake { .. }
                | Command::Store { .. }
                | Command::Tag { .. }
                | Command::Undo { .. }
                | Command::Unload { .. }
        )
    }

    /// Whether this command must skip the entry-heal chokepoint
    /// ([`forklift_core::util::heal_utils::heal_if_tainted`]) instead of refusing behind it while
    /// a durability taint stands.
    ///
    /// Exactly two commands are exempt, both because refusing behind the very chokepoint they
    /// exist to resolve or diagnose would be circular:
    ///
    /// * `heal` — the recovery verb the taint's own refusal message names as the next step
    ///   (`heal_utils::DURABILITY_TAINT_NEXT_STEP`). If it were itself blocked by the chokepoint
    ///   it promises to resolve, a standing taint would have no way out at all.
    /// * `audit` — a read-only diagnostic. It verifies signatures and (optionally) re-hashes
    ///   present content; it never writes an object, a ref, or a taint file itself, so trusting
    ///   an existence check while a taint stands carries none of the "durable reference to
    ///   unproven bytes" risk the chokepoint exists to prevent (see
    ///   `commands::audit::handle_command` — every path through it is a read).
    ///
    /// Every other command keeps refusing. This is the one place that decides the exemption, so
    /// it stays greppable and independently testable — see `cli.rs`'s own tests and
    /// `tests/cli.rs`'s `heal`/`audit`-while-tainted integration coverage.
    ///
    /// # Returns
    /// * `true`  - The chokepoint must be bypassed for this command.
    /// * `false` - The command refuses normally while a taint stands.
    pub fn bypasses_taint_heal(&self) -> bool {
        matches!(self, Command::Heal | Command::Audit { .. })
    }

    /// Whether the command refuses while an operation interrupted before it finished is recorded
    /// in the bay (see `operation_utils`): every command that changes the bay's files, staging or
    /// position, or commits staged content. The staging such an operation leaves may be
    /// incomplete, and committing it could record the removal of every tracked file. `heal` is
    /// the way out; commands that touch only refs of other pallets, the shared store or
    /// metadata stay usable. Of the pallet family only the listing runs: `pallet delete` writes
    /// the bay's undo journal, and `pallet restore` re-creates a ref a checkout may stand on.
    ///
    /// # Returns
    /// * `true`  - The command refuses while such an operation is recorded.
    /// * `false` - The command runs as usual.
    pub fn refused_during_interrupted_operation(&self) -> bool {
        matches!(
            self,
            Command::Pick { .. }
                | Command::Consolidate { .. }
                | Command::Git { action: GitAction::Import { .. } }
                | Command::Lift { .. }
                | Command::Load { .. }
                | Command::Lower { .. }
                | Command::Haul { .. }
                | Command::Pallet { action: Some(PalletAction::Create { .. } | PalletAction::Delete { .. } | PalletAction::Restore { .. }) }
                | Command::Park { .. }
                | Command::Remove { .. }
                | Command::Restore { .. }
                | Command::Scope { action: Some(ScopeAction::Fetch { .. } | ScopeAction::Narrow { .. } | ScopeAction::Prune { .. }) }
                | Command::Store { action: Some(StoreAction::Evict { .. }) }
                | Command::Shift { .. }
                | Command::Stack { .. }
                | Command::Undo { .. }
                | Command::Unload { .. }
        )
    }

    /// Check whether the command mutates the warehouse (inventory, refs, objects) and
    /// therefore has to hold the warehouse lock while it runs (see `WarehouseLock`). A command
    /// that writes the working directory belongs here too, unless it takes the lock itself, as
    /// `clone` does for the warehouse it creates: taking a bay's lock clears its chunked-file
    /// assembly folder, which is safe only while every assembler holds the lock.
    ///
    /// # Returns
    /// * `true`  - If the command must run under the warehouse lock.
    /// * `false` - If the command is read-only (or does not touch a warehouse at all).
    pub fn requires_warehouse_lock(&self) -> bool {
        matches!(
            self,
            Command::Bay { .. }
                | Command::Pick { .. }
                | Command::Consolidate { .. }
                | Command::Deliver { .. }
                | Command::Heal
                | Command::Git { action: GitAction::Import { .. } | GitAction::Mirror }
                | Command::Lift { .. }
                | Command::Load { .. }
                | Command::Lower { .. }
                | Command::Haul { .. }
                | Command::Manifest { .. }
                | Command::Office { .. }
                | Command::Pallet { .. }
                | Command::Park { .. }
                | Command::Remove { .. }
                | Command::Remote { action: Some(_) }
                | Command::Restore { .. }
                | Command::Scope { action: Some(ScopeAction::Fetch { .. } | ScopeAction::Narrow { .. } | ScopeAction::Prune { .. }) }
                | Command::Store { action: Some(StoreAction::Compact { .. } | StoreAction::Evict { .. }) }
                | Command::Shift { .. }
                | Command::Stack { .. }
                | Command::Tag { .. }
                | Command::Undo { .. }
                | Command::Unload { .. }
        )
    }

    /// Whether the command holds the store-access lock shared for its whole run (see
    /// `StoreAccessLock`), which orders its writes to the shared object store against every
    /// operation that deletes objects on a liveness or scope decision in another process.
    ///
    /// Every command that publishes a root another process can see — a pallet head, a staged
    /// shard, a parked parcel, consolidation state — must be on [`Self::requires_warehouse_lock`]
    /// for this to cover it; a new such command belongs there. `clone` is the one command off
    /// that list that sets a pallet head, and it writes into a store it has just created, which
    /// no other process uses yet.
    ///
    /// `store compact`, `store evict` and `scope prune` are excluded because they host deleters,
    /// which take the exclusive side; their own shared hold would refuse them. They take the side
    /// they need themselves: a full repack, evict and scope prune the exclusive one, incremental
    /// compaction the shared one, tried.
    ///
    /// # Returns
    /// * `true`  - Take the shared hold right after the warehouse lock.
    /// * `false` - A read-only command, or one that hosts a deleter.
    pub fn holds_store_access(&self) -> bool {
        self.requires_warehouse_lock()
            && !matches!(
                self,
                Command::Store { action: Some(StoreAction::Compact { .. } | StoreAction::Evict { .. }) }
                    | Command::Scope { action: Some(ScopeAction::Prune { .. }) }
            )
    }

    /// Whether finishing this command should kick off background object-store maintenance
    /// (git's `gc --auto`): the mutating commands that add loose objects, so the store stays
    /// packed without the user remembering to `store compact`. `store compact` itself is excluded
    /// (or it would re-trigger), `git import` is (it compacts on its own), and so is `store evict`,
    /// which only removes objects. `lift` and `lower` are
    /// included on purpose: both bring in objects from a remote. The actual work is
    /// still gated on a cheap loose/pack threshold, so triggering here is only a *check*.
    ///
    /// # Returns
    /// * `true`  - Check for (and maybe spawn) background maintenance after this command.
    /// * `false` - Do not (a read-only, setup, or self-compacting command).
    pub fn triggers_auto_maintenance(&self) -> bool {
        self.requires_warehouse_lock()
            && !matches!(
                self,
                Command::Store { action: Some(StoreAction::Compact { .. } | StoreAction::Evict { .. }) }
                    | Command::Git { action: GitAction::Import { .. } }
            )
    }

    /// Whether the command produces read-only, potentially long output that should go
    /// through a pager on a terminal. Git pages its log/diff/show/blame family and never a
    /// command that mutates or might prompt (a passphrase behind a pager would deadlock), so
    /// this is exactly the read-only display commands.
    ///
    /// # Returns
    /// * `true`  - Page the output on a terminal (`history`, `diff`, `store peek`, `show`, `blame`, `audit`).
    /// * `false` - Print straight through (mutating, interactive, or short-output commands).
    pub fn pages_output(&self) -> bool {
        matches!(
            self,
            Command::History { .. }
                | Command::Query { .. }
                | Command::Diff { .. }
                | Command::Store { action: Some(StoreAction::Peek { .. }) }
                | Command::Show { .. }
                | Command::Blame { .. }
                | Command::Audit { .. }
        )
    }

    /// The name this command records in the undo journal (§7.8), or `None` if it is not
    /// journaled. Only state-changing operations `undo` knows how to reverse are listed;
    /// their pre-operation state is snapshotted so `undo` can restore it. Pure-staging
    /// (`load`/`remove`/`unload`/`restore`), trust and remote commands are intentionally excluded.
    /// A pallet delete is journaled as well, but writes its own entry inside the removal's ref-lock
    /// hold (`pallet delete`), so it is not listed here.
    pub fn journal_op(&self) -> Option<&'static str> {
        match self {
            Command::Stack { .. } => Some("stack"),
            Command::Consolidate { .. } => Some("consolidate"),
            Command::Pick { .. } => Some("pick"),
            Command::Shift { .. } => Some("shift"),
            _ => None,
        }
    }
}

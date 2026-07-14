use serde::Serialize;
use forklift_core::builder::object::loose_object_builder::LooseObjectBuilder;
use forklift_core::model::blob::Blob;
use forklift_core::util::inventory_utils;
use crate::output::{self, CommandOutput};

/// Handle the conflicts command (§7.4): report the files left in conflict by an
/// unresolved consolidation, as structured records rather than marker soup — agents
/// resolve merges well when given the three sides directly.
///
/// Each conflicted file's working copy carries diff3 markers; this reconstructs the
/// three full versions (base, ours, theirs) from them and stores each as a content-
/// addressed blob, so `--json` yields `{ path, base, ours, theirs }` addresses the
/// resolver can fetch and diff. A whole-file or binary conflict has no markers and is
/// reported as such.
///
/// # Returns
/// * `Ok(())`      - Whether or not there are conflicts (an empty list is a valid
///                   answer — an agent checks the list, not an error).
/// * `Err(String)` - If the inventory or a working file could not be read.
pub fn handle_command() -> Result<(), String> {
    let paths = inventory_utils::list_conflict_paths()?;

    let mut conflicts = Vec::new();

    for path in paths {
        conflicts.push(build_conflict(&path)?);
    }

    output::emit("conflicts", &ConflictReport { conflicts });

    Ok(())
}

/// Build the structured record of one conflicted file: its three sides as content
/// addresses when the working copy carries diff3 markers, or a marker-less note.
fn build_conflict(path: &str) -> Result<Conflict, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        // A conflict entry whose working file is gone (e.g. a delete/modify conflict
        // the user removed) is still worth surfacing — just without content.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Conflict { path: path.to_string(), markers: false, base: None, ours: None, theirs: None });
        }
        Err(error) => return Err(format!("Error while reading \"{}\": {}", path, error)),
    };

    let Some(sides) = reconstruct_sides(&bytes) else {
        return Ok(Conflict { path: path.to_string(), markers: false, base: None, ours: None, theirs: None });
    };

    Ok(Conflict {
        path: path.to_string(),
        markers: true,
        base: Some(store_blob(sides.base)?),
        ours: Some(store_blob(sides.ours)?),
        theirs: Some(store_blob(sides.theirs)?),
    })
}

/// The three reconstructed sides of a diff3-marked file.
struct Sides {
    ours: Vec<u8>,
    base: Vec<u8>,
    theirs: Vec<u8>,
}

/// Which section of a diff3 hunk the walk is currently inside.
enum Section {
    /// Outside any conflict hunk: the line is common to all three sides.
    Common,
    Ours,
    Base,
    Theirs,
}

/// Reconstruct the full ours/base/theirs versions from a file carrying forklift's
/// diff3 markers (`merge_utils::conflict_chunk`): lines outside a hunk belong to all
/// three sides, lines inside each section belong to that side. Returns `None` when the
/// file has no conflict markers at all (a whole-file or binary conflict).
fn reconstruct_sides(bytes: &[u8]) -> Option<Sides> {
    let text = std::str::from_utf8(bytes).ok()?;

    let mut sides = Sides { ours: Vec::new(), base: Vec::new(), theirs: Vec::new() };
    let mut section = Section::Common;
    let mut saw_marker = false;

    // Split keeping line terminators so the reconstruction is byte-exact.
    for line in text.split_inclusive('\n') {
        let trimmed = line.strip_suffix('\n').unwrap_or(line);

        if trimmed.starts_with("<<<<<<< ") {
            section = Section::Ours;
            saw_marker = true;
        } else if trimmed == "||||||| base" {
            section = Section::Base;
        } else if trimmed == "=======" {
            section = Section::Theirs;
        } else if trimmed.starts_with(">>>>>>> ") {
            section = Section::Common;
        } else {
            match section {
                Section::Common => {
                    sides.ours.extend_from_slice(line.as_bytes());
                    sides.base.extend_from_slice(line.as_bytes());
                    sides.theirs.extend_from_slice(line.as_bytes());
                }
                Section::Ours => sides.ours.extend_from_slice(line.as_bytes()),
                Section::Base => sides.base.extend_from_slice(line.as_bytes()),
                Section::Theirs => sides.theirs.extend_from_slice(line.as_bytes()),
            }
        }
    }

    saw_marker.then_some(sides)
}

/// Store content as a blob object and return its hash (the content address). Idempotent
/// — a side identical to an existing object dedupes.
fn store_blob(content: Vec<u8>) -> Result<String, String> {
    let mut object = LooseObjectBuilder::build_blob(&Blob { content });
    object.store()?;

    Ok(object.hash)
}

/// The conflict report: every file an unresolved consolidation left in conflict.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct ConflictReport {
    conflicts: Vec<Conflict>,
}

/// One conflicted file. When the working copy carries diff3 markers, the three sides
/// are content addresses (blob hashes) a resolver can fetch; otherwise `markers` is
/// false and the sides are absent (a whole-file or binary conflict).
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct Conflict {
    path: String,
    markers: bool,

    /// The common ancestor's version (content address).
    #[serde(skip_serializing_if = "Option::is_none")]
    base: Option<String>,

    /// The current pallet's version (content address).
    #[serde(skip_serializing_if = "Option::is_none")]
    ours: Option<String>,

    /// The consolidated pallet's version (content address).
    #[serde(skip_serializing_if = "Option::is_none")]
    theirs: Option<String>,
}

impl CommandOutput for ConflictReport {
    fn render_human(&self) {
        if self.conflicts.is_empty() {
            println!("There are no unresolved conflicts.");
            return;
        }

        println!("Unresolved conflicts ({}):", self.conflicts.len());

        for conflict in &self.conflicts {
            if conflict.markers {
                println!(
                    "  {} (base {}, ours {}, theirs {})",
                    conflict.path,
                    short(&conflict.base),
                    short(&conflict.ours),
                    short(&conflict.theirs),
                );
            } else {
                println!("  {} (whole-file or binary conflict)", conflict.path);
            }
        }

        println!("\nResolve each file, \"load\" it, then \"stack\" to complete the consolidation.");
    }
}

/// A short (12-char) prefix of a content address, for the human listing.
fn short(hash: &Option<String>) -> &str {
    match hash {
        Some(hash) => &hash[..12.min(hash.len())],
        None => "—",
    }
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<(&'static str, schemars::Schema)> {
    vec![
        ("ConflictReport", schemars::schema_for!(ConflictReport)),
    ]
}

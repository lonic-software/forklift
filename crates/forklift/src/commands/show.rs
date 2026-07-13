use serde::Serialize;
use forklift_core::util::{object_utils, pallet_utils};
use crate::output::{self, CommandOutput};

/// Handle the `show` command: print a file's content at a revision in one invocation
/// (`git show <rev>:<path>`'s equivalent) — no separate "resolve the tree, find the blob,
/// peek the hash" round trip.
///
/// # Arguments
/// * `target` - The `<revision>:<path>` argument, split on the *first* `:` (a revision —
///              a pallet name, an `@`-qualified meta pallet, or a hash prefix — can never
///              contain `:`, so the split is unambiguous even when the path itself has one).
///
/// # Returns
/// * `Ok(())`      - If the file was found and its content (or binary/chunked metadata)
///                   was printed.
/// * `Err(String)` - If the target is malformed, the revision does not resolve, or the
///                   path does not exist in that revision's tree.
pub fn handle_command(target: &str) -> Result<(), String> {
    let (revision, path) = target.split_once(':').ok_or_else(|| format!(
        "\"{}\" is not \"<revision>:<path>\" — pass a revision, a colon, then the path.",
        target
    ))?;

    if revision.is_empty() {
        return Err(format!("\"{}\" has no revision before the \":\".", target));
    }

    if path.is_empty() {
        return Err(format!("\"{}\" has no path after the \":\".", target));
    }

    let parcel_hash = pallet_utils::resolve_revision(revision)?;
    let tree_hash = object_utils::load_parcel(&parcel_hash)?.tree_hash;

    let Some((hash, item_type)) = object_utils::resolve_tree_file(&tree_hash, path)? else {
        return Err(format!(
            "\"{}\" was not found at revision \"{}\" ({}).",
            path, revision, parcel_hash
        ));
    };

    // A chunked (large) file's tree-entry hash is a recipe, not a blob: report its metadata
    // without assembling the file — the same honest-binary line `diff` draws for chunked
    // content, never a multi-GB read to answer a "show me this file" request.
    let shown = if item_type.is_chunked() {
        let recipe = object_utils::load_recipe(&hash)?;

        Shown {
            revision: parcel_hash,
            path: path.to_string(),
            hash,
            binary: true,
            content: None,
            size: recipe.total_size,
            content_hash: Some(recipe.content_hash),
            chunk_count: Some(recipe.chunks.len()),
        }
    } else {
        let blob = object_utils::load_blob(&hash)?;
        let size = blob.content.len() as u64;

        // Text means NUL-free *and* valid UTF-8 (see `output::blob_text`) — never a lossy
        // conversion that could mangle non-UTF-8 bytes into fake text with no signal that it
        // happened.
        let text = output::blob_text(&blob.content);

        Shown {
            revision: parcel_hash,
            path: path.to_string(),
            hash,
            binary: text.is_none(),
            content: text.map(str::to_string),
            size,
            content_hash: None,
            chunk_count: None,
        }
    };

    output::emit("show", &shown);

    Ok(())
}

/// A `show` result: a file's content at a revision, or — when it is binary or a chunked
/// large file — the metadata that explains why there is no `content` instead. The public
/// JSON schema (a change here is a schema change; see `crate::output::SCHEMA_VERSION`).
#[derive(Serialize)]
struct Shown {
    /// The resolved parcel hash the revision argument named (a pallet head, a meta-pallet
    /// head, or the parcel a hash prefix matched) — never the raw revision argument, so a
    /// caller always gets the exact, disambiguated parcel this content came from.
    revision: String,

    /// The path, as given (already validated to exist in the revision's tree).
    path: String,

    /// The tree entry's own object hash: a blob hash for plain content, a recipe hash for a
    /// chunked large file.
    hash: String,

    /// Whether the content is not shown as text: either non-text bytes (a NUL byte anywhere,
    /// or invalid UTF-8 — see `output::blob_text`) or a chunked large file, which is never
    /// assembled just to answer `show`.
    binary: bool,

    /// The file's content as text. Present only when `binary` is `false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,

    /// The file's size in bytes: the blob length, or a chunked file's assembled total size.
    size: u64,

    /// A chunked file's whole-content hash (advisory until assembly; see [`object_utils`]'s
    /// `Recipe`). Present only for a chunked file.
    #[serde(skip_serializing_if = "Option::is_none")]
    content_hash: Option<String>,

    /// A chunked file's chunk count. Present only for a chunked file.
    #[serde(skip_serializing_if = "Option::is_none")]
    chunk_count: Option<usize>,
}

impl CommandOutput for Shown {
    fn render_human(&self) {
        if self.binary {
            match (&self.content_hash, self.chunk_count) {
                (Some(content_hash), Some(chunk_count)) => println!(
                    "(chunked file, {} bytes across {} chunks, content {})",
                    self.size, chunk_count, content_hash
                ),
                _ => println!("(binary file, {} bytes)", self.size),
            }

            return;
        }

        if let Some(content) = &self.content {
            print!("{}", content);
        }
    }
}

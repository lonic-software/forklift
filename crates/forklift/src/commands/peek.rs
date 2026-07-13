use serde::Serialize;
use forklift_core::enums::object::parsed_object::ParsedObject;
use forklift_core::enums::object_type::ObjectType;
use forklift_core::model::blob::Blob;
use forklift_core::model::chunk::Chunk;
use forklift_core::util::path_utils::WarehousePath;
use forklift_core::model::parcel::Parcel;
use forklift_core::model::parcel_action::ParcelAction;
use forklift_core::model::recipe::Recipe;
use forklift_core::model::tree_item::TreeItem;
use forklift_core::parser;
use forklift_core::parser::inventory::inventory_parser;
use forklift_core::util::file_utils;
use crate::output::{self, CommandOutput};

const PARCEL_FIELD_TREE: &str =   "tree  ";
const PARCEL_FIELD_PARENT: &str = "parent";
const PARCEL_FIELD_ACTION: &str = "action";

/// Handle the `peek` command.
///
/// # Arguments
/// * `inventory` - The folder whose inventory to peek into (instead of an object).
/// * `object`    - The hash of the object to peek into.
/// * `verbose`   - Whether to print the full details of inventory entries.
///
/// # Returns
/// * `Ok(())`      - If the command was handled successfully.
/// * `Err(String)` - If an error occurred while handling the command.
pub fn handle_command(inventory: Option<String>,
                      object: Option<String>,
                      verbose: bool) -> Result<(), String> {
    match (inventory, object) {
        (Some(path), _) => peek_inventory(&path, verbose),
        (None, Some(hash)) => peek_object(&hash),
        // Clap requires the object hash whenever --inventory is absent.
        (None, None) => unreachable!("the CLI definition guarantees one of the targets"),
    }
}

/// Peek into the inventory of the given folder in the working directory.
///
/// # Arguments
/// * `path`    - The path to the folder to peek into.
/// * `verbose` - Whether to print the full details of the inventory.
///
/// # Returns
/// * `Ok(())`      - If the inventory was peeked into successfully.
/// * `Err(String)` - If an error occurred while peeking into the inventory.
fn peek_inventory(path: &str, verbose: bool) -> Result<(), String> {
    let warehouse_path = WarehousePath::from_user_input(path)?;
    let (_, saved_inventory) = file_utils::retrieve_inventory_by_key(warehouse_path.as_key())?;
    let inventory = inventory_parser::parse_inventory(&saved_inventory)?;

    if output::is_json() {
        let items = inventory.get_items()
            .map(|(name, item)| PeekInventoryItem {
                state: item.state.to_string(),
                hash: item.hash.clone(),
                name: name.clone(),
            })
            .collect();

        output::emit("peek", &PeekInventory { items });

        return Ok(());
    }

    if inventory.get_items().len() == 0 {
        println!("No items in the inventory.");
    } else {
        for (name, item) in inventory.get_items() {
            if verbose {
                println!("{}\n", item);
            } else {
                println!("{} {} {}", item.state, item.hash, name);
            }
        }
    }

    Ok(())
}

/// A `--json` inventory peek.
#[derive(Serialize)]
struct PeekInventory {
    items: Vec<PeekInventoryItem>,
}

/// One inventory entry.
#[derive(Serialize)]
struct PeekInventoryItem {
    state: String,
    hash: String,
    name: String,
}

impl CommandOutput for PeekInventory {
    fn render_human(&self) {}
}

/// Peek into the object with the given hash.
///
/// # Arguments
/// * `hash` - The hash of the object to peek into.
///
/// # Returns
/// * `Ok(())`      - If the object was peeked into successfully.
/// * `Err(String)` - If an error occurred while peeking into the object.
fn peek_object(hash: &str) -> Result<(), String> {
    let object_bytes = file_utils::retrieve_object_by_hash(hash)?;
    let object = parser::object::loose_object_parser::parse(&object_bytes)?;

    if output::is_json() {
        return peek_object_json(object);
    }

    print_header(&object.get_type());

    match object {
        ParsedObject::Blob(blob) => peek_blob(blob),
        ParsedObject::Tree(tree) => peek_tree(tree),
        ParsedObject::Parcel(parcel) => peek_parcel(parcel),
        ParsedObject::Recipe(recipe) => peek_recipe(recipe),
        ParsedObject::Chunk(chunk) => peek_chunk(chunk),
    }?;

    Ok(())
}

/// Emit a parsed object as structured JSON.
fn peek_object_json(object: ParsedObject) -> Result<(), String> {
    let object_type = object.get_type().to_string();

    let peeked = match object {
        ParsedObject::Blob(blob) => {
            // Text means NUL-free *and* valid UTF-8 (see `output::blob_text`) — reported
            // honestly instead of mangling the bytes through a lossy UTF-8 conversion, which
            // used to silently corrupt binary content into fake text with no signal that it
            // happened.
            let text = output::blob_text(&blob.content);

            PeekObject {
                content: text.map(str::to_string),
                binary: Some(text.is_none()),
                ..PeekObject::empty(object_type)
            }
        }
        ParsedObject::Tree(tree) => {
            let entries = tree.get_files().chain(tree.get_subtrees())
                .map(|(_, item)| PeekTreeEntry {
                    item_type: item.item_type.get_name_for_peek().to_string(),
                    hash: item.hash.clone(),
                    name: item.name.clone(),
                })
                .collect();

            PeekObject { entries, ..PeekObject::empty(object_type) }
        }
        ParsedObject::Parcel(parcel) => {
            let actions = parcel.actions.iter().map(|action| PeekAction {
                action: action.action.get_name_for_peek().to_string(),
                operator: action.operator.identifier.clone(),
                timestamp: action.timestamp.to_rfc3339(),
                description: action.description.clone().filter(|d| !d.is_empty()),
            }).collect();

            PeekObject {
                tree: Some(parcel.tree_hash.clone()),
                parents: parcel.parents.clone(),
                actions,
                description: parcel.description.clone(),
                ..PeekObject::empty(object_type)
            }
        }
        ParsedObject::Recipe(recipe) => {
            let chunks = recipe.chunks.iter()
                .map(|chunk| PeekChunk { hash: chunk.hash.clone(), size: chunk.size })
                .collect();

            PeekObject {
                content_hash: Some(recipe.content_hash.clone()),
                total_size: Some(recipe.total_size),
                chunks,
                ..PeekObject::empty(object_type)
            }
        }
        ParsedObject::Chunk(chunk) => PeekObject {
            total_size: Some(chunk.content.len() as u64),
            ..PeekObject::empty(object_type)
        },
    };

    output::emit("peek", &peeked);

    Ok(())
}

/// A `--json` object peek: the fields relevant to the object's type are set.
#[derive(Serialize)]
struct PeekObject {
    object_type: String,

    /// A blob's content as text.
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,

    /// Whether a blob is binary — contains a NUL byte, or is not valid UTF-8 (see
    /// `output::blob_text`) — `content` is then omitted rather than carrying lossily-mangled
    /// bytes. Absent for every other object type.
    #[serde(skip_serializing_if = "Option::is_none")]
    binary: Option<bool>,

    /// A tree's entries.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    entries: Vec<PeekTreeEntry>,

    /// A parcel's root tree hash.
    #[serde(skip_serializing_if = "Option::is_none")]
    tree: Option<String>,

    /// A parcel's parent hashes.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    parents: Vec<String>,

    /// A parcel's actions.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    actions: Vec<PeekAction>,

    /// A parcel's description.
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,

    /// A recipe's whole-file content hash.
    #[serde(skip_serializing_if = "Option::is_none")]
    content_hash: Option<String>,

    /// A recipe's total assembled size, or a chunk's payload size.
    #[serde(skip_serializing_if = "Option::is_none")]
    total_size: Option<u64>,

    /// A recipe's ordered chunk list.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    chunks: Vec<PeekChunk>,
}

impl PeekObject {
    /// An otherwise-empty peek of the given type — every type-specific field defaulted, so each
    /// arm sets only the fields relevant to its object type via struct update.
    fn empty(object_type: String) -> PeekObject {
        PeekObject {
            object_type,
            content: None,
            binary: None,
            entries: Vec::new(),
            tree: None,
            parents: Vec::new(),
            actions: Vec::new(),
            description: None,
            content_hash: None,
            total_size: None,
            chunks: Vec::new(),
        }
    }
}

/// One entry of a tree object.
#[derive(Serialize)]
struct PeekTreeEntry {
    item_type: String,
    hash: String,
    name: String,
}

/// One chunk of a recipe object.
#[derive(Serialize)]
struct PeekChunk {
    hash: String,
    size: u64,
}

/// One action of a parcel object.
#[derive(Serialize)]
struct PeekAction {
    action: String,
    operator: String,
    timestamp: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

impl CommandOutput for PeekObject {
    fn render_human(&self) {}
}

/// Print the peek header (e.g. object type) to stdout.
///
/// # Arguments
/// * `object_type` - The type of the object to print.
fn print_header(object_type: &ObjectType) {
    println!("Type: {}\n\nContent:", object_type);
}

/// Print the content of the given blob to stdout — or, for a binary blob, a short
/// notice instead of raw bytes (the same NUL-free-and-valid-UTF-8 text test `show` and
/// `peek --json` use — see `output::blob_text`).
///
/// # Arguments
/// * `object` - The blob to print.
///
/// # Returns
/// * `Ok(())` - Always (a binary blob is reported, never an error).
fn peek_blob(object: Blob) -> Result<(), String> {
    let Some(text) = output::blob_text(&object.content) else {
        println!("(binary blob, {} bytes)", object.content.len());
        return Ok(());
    };

    println!("{}", text);

    Ok(())
}

/// Print details of the given recipe (a chunked file's chunk index) to stdout.
///
/// # Arguments
/// * `recipe` - The recipe to print.
///
/// # Returns
/// * `Ok(())` - Always (nothing here can fail).
fn peek_recipe(recipe: Recipe) -> Result<(), String> {
    println!("content-hash {}", recipe.content_hash);
    println!("total-size   {}", recipe.total_size);
    println!("chunks       {}", recipe.chunks.len());

    for (index, chunk) in recipe.chunks.iter().enumerate() {
        println!("  {:>6} {} {}", index, chunk.hash, chunk.size);
    }

    Ok(())
}

/// Print details of the given chunk (a leaf byte-range of a chunked file) to stdout. The raw
/// bytes are not printed — a chunk is binary — only its size.
///
/// # Arguments
/// * `chunk` - The chunk to print.
///
/// # Returns
/// * `Ok(())` - Always.
fn peek_chunk(chunk: Chunk) -> Result<(), String> {
    println!("(binary chunk, {} bytes)", chunk.content.len());

    Ok(())
}

/// Print details of the given tree to stdout.
///
/// # Arguments
/// * `tree` - The tree to print.
///
/// # Returns
/// * `Ok(())`      - If the tree was printed successfully.
/// * `Err(String)` - If an error occurred while printing the tree.
fn peek_tree(tree: TreeItem) -> Result<(), String> {
    for (_, file) in tree.get_files() {
        print_tree_item(file);
    }

    for (_, subtree) in tree.get_subtrees() {
        print_tree_item(subtree);
    }

    Ok(())
}

/// Print details of the given tree item to stdout.
///
/// # Arguments
/// * `tree_item` - The tree item to print.
fn print_tree_item(tree_item: &TreeItem) {
    println!("{} {}\t{}", tree_item.item_type.get_name_for_peek(), tree_item.hash, tree_item.name);
}

/// Print details of the given parcel to stdout.
///
/// # Arguments
/// * `parcel` - The parcel to print.
///
/// # Returns
/// * `Ok(())`      - If the parcel was printed successfully.
/// * `Err(String)` - If an error occurred while printing the parcel.
fn peek_parcel(parcel: Parcel) -> Result<(), String> {
    println!("{} {}", PARCEL_FIELD_TREE, parcel.tree_hash);

    for parent in parcel.parents {
        println!("{} {}", PARCEL_FIELD_PARENT, parent);
    }

    for action in parcel.actions {
        print_parcel_action(&action);
    }

    if let Some(description) = &parcel.description {
        println!("\n{}", description);
    }

    Ok(())
}

/// Print details of the given parcel action to stdout.
///
/// # Arguments
/// * `action` - The parcel action to print.
fn print_parcel_action(action: &ParcelAction) {
    println!(
        "{} {} {} {}",
        PARCEL_FIELD_ACTION,
        action.action.get_name_for_peek(),
        action.operator.identifier,
        action.timestamp.to_rfc3339(),
    );

    action.description.as_ref().inspect(|description| {
        if description.len() > 0 {
            println!("{}\n", **description);
        }
    });
}

//! The origin record: the remote a partial warehouse (cloned with `--only` or `--cache`) was
//! cloned from, and the one rule every remote client of a warehouse goes through.
//!
//! A partial warehouse lacks content its history names, and only its origin was proved to hold
//! it: the sparse fetch skipped out-of-scope content against the origin, and a cache holds the
//! objects it freed or never fetched by hash, refetched from the origin on demand. Talking to any
//! other remote from here could fetch the wrong history into a ledger, decide what to free by heads
//! the origin does not have, or lift onto a remote that lacks what this warehouse never verified
//! there. So in a partial warehouse [`checked_remote`] refuses unless `remote.url` is the origin.
//!
//! The record is a [`record_utils`] file in the shared forklift root holding one entry, the URL. It
//! is not configuration: no `config` command reaches it, so the origin cannot be edited to agree
//! with whatever remote is configured. `prepare_warehouse` writes it before the marker of a partial
//! warehouse, so no partial marker stands without an origin, and before the clone fetches
//! anything; afterwards only a move of the origin rewrites it ([`write_locked`]), which is also how a
//! missing one is recorded again. A full warehouse
//! has none: it holds its whole closure and may talk to any remote.

use std::path::{Path, PathBuf};
use crate::error::{CoreError, RefusalCode};
use crate::globals;
use crate::util::lock_utils::RefLock;
use crate::util::{config_utils, file_utils, origin_move, record_utils, scope_utils};

/// The record's file name in the shared forklift root, and its [`record_utils`] kind and version.
pub const FILE_NAME: &str = "origin";
const RECORD: &str = "origin";
const VERSION: u32 = 1;

fn record_path(forklift: &Path) -> PathBuf {
    forklift.join(FILE_NAME)
}

/// The origin recorded in the warehouse in scope, or none when the file is absent. A file that
/// does not parse, or holds anything but one URL, is an error, never no origin.
pub fn read_origin() -> Result<Option<String>, String> {
    let path = record_path(&globals::forklift_root());

    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("Error while reading \"{}\": {}", path.to_string_lossy(), e)),
    };

    // Coded, so a server answers it as damage only its operator mends. A move records the origin
    // again, checking first that its server holds what this warehouse lacks.
    let damaged = |message: String| -> String {
        CoreError::refusal(RefusalCode::OriginDamaged, message, "forklift remote move <url>").into()
    };

    let entries = record_utils::from_bytes(&bytes, RECORD, VERSION)
        .map_err(|e| damaged(format!("The file \"{}\" {}", path.to_string_lossy(), e.describe())))?;

    match <[String; 1]>::try_from(entries) {
        Ok([url]) if !url.is_empty() => Ok(Some(url)),
        Ok(_) => Err(damaged(format!("The file \"{}\" records an empty origin.", path.to_string_lossy()))),
        Err(entries) => Err(damaged(format!(
            "The file \"{}\" holds {} entries where it should hold one, the origin's URL.",
            path.to_string_lossy(), entries.len()
        ))),
    }
}

/// Record `url` as the origin of the warehouse in `forklift`, atomically and synced with its
/// folder: for `prepare_warehouse`, before the marker, so no other writer can exist yet and no
/// lock is needed, and through [`write_locked`] for a move.
pub(crate) fn write_origin(forklift: &Path, url: &str) -> Result<(), String> {
    if url.is_empty() {
        return Err("A partial warehouse's origin must be a remote URL.".to_string());
    }

    let path = record_path(forklift);

    file_utils::replace_file_atomically(&path, &record_utils::to_bytes(RECORD, VERSION, [url]))
        .map_err(|e| format!("Error while recording the origin \"{}\": {}", path.to_string_lossy(), e))
}

/// Rewrite the origin of the warehouse in scope as `url`, for the move that owns the recorded
/// intent once it is rewriting to `url` ([`origin_move::require_rewriting`]): the only writer
/// after `prepare_warehouse`.
pub fn write_locked(lock: &RefLock, token: &origin_move::MoveToken, url: &str) -> Result<(), String> {
    origin_move::require_rewriting(lock, token, url)?;
    write_origin(&globals::forklift_root(), url)
}

/// Record `url` as the origin of the warehouse in scope, for a test fixture that builds a partial
/// warehouse by hand.
#[doc(hidden)]
pub fn write_for_tests(url: &str) {
    write_origin(&globals::forklift_root(), url).unwrap();
}

/// The remote this warehouse may talk to, as its URL and token: the configured `remote.url` and
/// `remote.token`, and in a partial warehouse only when that URL is the recorded origin.
///
/// # Returns
/// * `Ok((url, token))` - The remote to dial.
/// * `Err(CoreError)`   - An `origin_move_pending` refusal while a move of the origin has not
///   finished; no remote is configured; in a partial warehouse, a `non_origin_remote`
///   refusal when the configured remote is not the origin or no origin is recorded; or the config,
///   the mode or the origin record cannot be read.
pub fn checked_remote() -> Result<(String, Option<String>), CoreError> {
    checked_remote_for(scope_utils::content_may_be_absent)
}

/// [`checked_remote`] judged by the warehouse mode alone, from the marker, never the fetch scope:
/// for `heal --fetch-scope`, which dials to replace a fetch scope that cannot be read. Every other
/// command reads the fetch scope first, so a damaged one refuses before it dials.
///
/// # Returns
/// * `Ok((url, token))` - The remote to dial.
/// * `Err(CoreError)`   - As for [`checked_remote`], with the marker read instead of the scope.
pub fn checked_remote_by_mode() -> Result<(String, Option<String>), CoreError> {
    checked_remote_for(|| Ok(crate::util::warehouse_utils::warehouse_mode()? != crate::util::warehouse_utils::WarehouseMode::Full))
}

/// [`checked_remote`], with `partial` saying whether the warehouse may lack content. With no remote
/// configured, it is read only to name the recorded origin, and a failure to read it names none.
fn checked_remote_for(partial: impl FnOnce() -> Result<bool, String>) -> Result<(String, Option<String>), CoreError> {
    // A move of the origin is checking the new server against what this warehouse holds, or has
    // started rewriting which remote that is: no client talks to either until it finishes.
    origin_move::ensure_none_pending()?;

    let configured = config_utils::get_effective_value(config_utils::KEY_REMOTE_URL)?.map(|(value, _)| value);

    let Some(url) = configured else {
        // A partial warehouse whose origin reads can name the one remote it may be pointed at.
        let recorded = if partial().unwrap_or(false) { read_origin().ok().flatten() } else { None };
        return Err(format!(
            "No remote is configured for this warehouse. Set one with \"forklift config set {} {}\".",
            config_utils::KEY_REMOTE_URL, recorded.as_deref().unwrap_or("<url>")
        ).into());
    };

    if partial()? {
        let origin = recorded_origin(Some(&url))?;

        if !same_remote(&url, &origin) {
            return Err(non_origin_remote_refusal(&origin, &url));
        }
    }

    let token = config_utils::get_effective_value(config_utils::KEY_REMOTE_TOKEN)?.map(|(value, _)| value);

    Ok((url, token))
}

/// Whether two spellings of a remote's URL name the same remote, as the client dials it.
pub fn same_remote(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

/// The `non_origin_remote` refusal for a partial warehouse whose configured remote is not its
/// origin.
///
/// # Arguments
/// * `origin` - The recorded origin.
/// * `other`  - The configured remote.
pub fn non_origin_remote_refusal(origin: &str, other: &str) -> CoreError {
    let next_step = format!(
        "Point \"{}\" back at \"{}\" with \"forklift config set {} {}\", or clone \"{}\" into a new \
        directory.",
        config_utils::KEY_REMOTE_URL, origin, config_utils::KEY_REMOTE_URL, origin, other
    );

    CoreError::refusal(
        RefusalCode::NonOriginRemote,
        format!(
            "This is a partial warehouse, cloned from \"{}\", and the configured remote is \
            \"{}\". Only the origin was proved to hold the content this warehouse does not, so \
            Forklift talks to no other remote from here; nothing was changed.",
            origin, other
        ),
        next_step,
    )
}

/// The origin a partial warehouse records, or the `non_origin_remote` refusal a missing or damaged
/// record earns ([`missing_origin_refusal`], [`damaged_origin_refusal`]).
///
/// # Arguments
/// * `configured` - The configured `remote.url`, which the refusal's next step names when set.
pub fn recorded_origin(configured: Option<&str>) -> Result<String, CoreError> {
    match read_origin() {
        Ok(Some(origin)) => Ok(origin),
        Ok(None) => Err(missing_origin_refusal(configured)),
        Err(why) => Err(damaged_origin_refusal(&why, configured)),
    }
}

/// The `non_origin_remote` refusal for a partial warehouse with no origin recorded, which only a
/// damaged `.forklift` can be: preparing a partial warehouse writes the origin before its marker.
///
/// # Arguments
/// * `configured` - The configured `remote.url`, which the next step names when set.
pub fn missing_origin_refusal(configured: Option<&str>) -> CoreError {
    CoreError::refusal(
        RefusalCode::NonOriginRemote,
        format!(
            "This is a partial warehouse, but its origin record (\"{}\" in the warehouse's .forklift \
            folder) is missing, so Forklift cannot tell which remote holds the content this \
            warehouse does not; nothing was changed.",
            FILE_NAME
        ),
        record_again_step(configured),
    )
}

/// The `non_origin_remote` refusal for a partial warehouse whose origin record cannot be read.
///
/// # Arguments
/// * `why`        - Why the record cannot be read, as [`read_origin`] says.
/// * `configured` - The configured `remote.url`, which the next step names when set.
pub fn damaged_origin_refusal(why: &str, configured: Option<&str>) -> CoreError {
    // The record's own refusal, by its words alone: this one names the step.
    let why = crate::error::deframe(why).map_or_else(|| why.to_string(), |(_, human, _)| human.into_owned());
    CoreError::refusal(
        RefusalCode::NonOriginRemote,
        format!(
            "This is a partial warehouse, but its origin record cannot be read ({}), so Forklift \
            cannot tell which remote holds the content this warehouse does not; nothing was changed.",
            why.trim_end_matches('.')
        ),
        record_again_step(configured),
    )
}

/// The next step that records the origin again: a move to the configured remote, which checks
/// that server holds this warehouse's content before it records it.
fn record_again_step(configured: Option<&str>) -> String {
    format!(
        "Record the origin again with \"forklift remote move {}\", naming a server that holds this \
        warehouse's content, or clone the remote again into a new directory.",
        configured.map(|url| url.trim_end_matches('/')).unwrap_or("<url>")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enums::config_scope::ConfigScope;
    use crate::util::warehouse_utils::{self, WarehouseMode};

    fn scratch(name: &str) -> (PathBuf, globals::StorageRootScope) {
        let root = std::env::temp_dir().join(format!("forklift-origin-{}-{}", name, std::process::id()));
        let _ = file_utils::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
        let scope = globals::StorageRootScope::enter(&root);
        (root, scope)
    }

    fn set_url(url: &str) {
        config_utils::set_value(config_utils::KEY_REMOTE_URL, url, ConfigScope::Warehouse).unwrap();
    }

    fn refusal_code(error: CoreError) -> RefusalCode {
        match error {
            CoreError::Refusal { code, .. } => code,
            other => panic!("expected a refusal: {:?}", other),
        }
    }

    /// The record reads back what was written, an absent file is no origin, and a damaged file is
    /// an error rather than no origin.
    ///
    /// Revert: read a file that does not parse as `Ok(None)` → the last assertions go green on `Ok`.
    #[test]
    fn the_origin_record_round_trips_and_a_damaged_one_is_an_error() {
        let (root, _scope) = scratch("record");
        let path = globals::forklift_root().join(FILE_NAME);

        assert_eq!(read_origin().unwrap(), None);

        write_for_tests("http://origin.example");
        assert_eq!(read_origin().unwrap().as_deref(), Some("http://origin.example"));

        std::fs::write(&path, b"http://origin.example\n").unwrap();
        assert!(read_origin().unwrap_err().contains("no format header"));

        std::fs::write(&path, record_utils::to_bytes(RECORD, VERSION, ["http://a", "http://b"])).unwrap();
        assert!(read_origin().unwrap_err().contains("holds 2 entries"));

        std::fs::write(&path, record_utils::to_bytes(RECORD, VERSION, [""])).unwrap();
        assert!(read_origin().unwrap_err().contains("empty origin"));

        // Coded, naming the move that records it again; and a remote command's refusal over it
        // carries the record's words alone, not its frame's fields. Mutation: paste `why` as it
        // comes → the code and the detail's `null` show in the message → red.
        let damaged = read_origin().unwrap_err();
        let (code, _, step) = crate::error::deframe(&damaged).expect("framed");
        assert_eq!((code, step), ("origin_damaged", "forklift remote move <url>"));
        let CoreError::Refusal { code, message, .. } = damaged_origin_refusal(&damaged, None) else { panic!() };
        assert_eq!(code, RefusalCode::NonOriginRemote);
        assert!(message.contains("records an empty origin") && !message.contains("origin_damaged")
            && !message.contains("null") && !message.contains("forklift remote move <url>"), "{}", message);

        file_utils::remove_dir_all(&root).ok();
    }

    /// A partial warehouse dials only its origin, and refuses a missing origin; a full warehouse
    /// dials whatever is configured, with or without a record.
    ///
    /// Revert: skip the comparison → the non-origin rows go `Ok`; or gate on the cache mode alone
    /// → the sparse rows go `Ok`; or pass a missing origin → the missing rows go `Ok`; or compare
    /// the URLs byte for byte → the trailing-slash row refuses.
    #[test]
    fn only_the_origin_is_dialled_from_a_partial_warehouse() {
        let (root, _scope) = scratch("checked");
        let path = globals::forklift_root().join(FILE_NAME);

        for mode in [WarehouseMode::Cache, WarehouseMode::Sparse] {
            warehouse_utils::mark_for_test(mode);
            if mode == WarehouseMode::Sparse {
                scope_utils::set_fetch_scope(&scope_utils::PathScope::from_prefixes(["docs"])).unwrap();
            }

            set_url("http://origin.example");
            let _ = file_utils::remove_file(&path);
            assert_eq!(refusal_code(checked_remote().unwrap_err()), RefusalCode::NonOriginRemote, "{:?}: missing", mode);

            write_for_tests("http://origin.example");
            assert_eq!(checked_remote().unwrap().0, "http://origin.example", "{:?}", mode);

            set_url("http://origin.example/");
            assert_eq!(checked_remote().unwrap().0, "http://origin.example/", "{:?}: a trailing slash names the same remote", mode);

            set_url("http://other.example");
            let error = checked_remote().unwrap_err();
            let text = String::from(error.clone());
            assert_eq!(refusal_code(error), RefusalCode::NonOriginRemote, "{:?}: other", mode);
            assert!(text.contains("http://origin.example") && text.contains("http://other.example"), "{}", text);
        }

        file_utils::remove_file(globals::forklift_root().join("config").join("fetch-scope")).unwrap();
        warehouse_utils::mark_for_test(WarehouseMode::Full);
        assert_eq!(checked_remote().unwrap().0, "http://other.example");
        let _ = file_utils::remove_file(&path);
        assert_eq!(checked_remote().unwrap().0, "http://other.example");

        file_utils::remove_dir_all(&root).ok();
    }
}

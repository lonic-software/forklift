//! The write-time backstop behind the case-alias checks: a write to a path new to the working
//! directory creates it exclusively and never removes what is there first, so an occupant the
//! up-front checks missed — reached through another spelling on a case-insensitive file system,
//! or simply present — makes the write fail and is left intact.
//!
//! The writers resolve paths against the process's working directory, so this file holds a single
//! test that owns it.

use std::path::Path;

use forklift_core::builder::object::loose_object_builder::LooseObjectBuilder;
use forklift_core::enums::dir_entry_type::DirEntryType;
use forklift_core::globals::StorageRootScope;
use forklift_core::model::blob::Blob;
use forklift_core::util::object_utils::{self, IngestMode};
use forklift_core::util::shift_utils;

mod common;
use common::forklift_command;

/// A writer of new paths under test, given the warehouse path to create.
type Writer<'a> = (&'a str, Box<dyn Fn(&str) -> Result<(), String> + 'a>);

#[test]
fn a_write_to_a_new_path_fails_on_any_occupant_and_leaves_it_intact() {
    let base = std::env::temp_dir().join(format!("forklift-case-alias-backstop-{}", std::process::id()));
    let root = base.join("warehouse");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&root).unwrap();

    let prepared = forklift_command()
        .arg("prepare")
        .current_dir(&root)
        .env("FORKLIFT_GLOBAL_CONFIG", base.join("global-config.toml"))
        .output()
        .unwrap();
    assert!(prepared.status.success(), "{}", String::from_utf8_lossy(&prepared.stderr));

    std::env::set_current_dir(&root).unwrap();
    let _scope = StorageRootScope::enter(&root);

    let blob = store_blob(b"tracked\n");
    let link_target = store_blob(b"somewhere");
    let big_source = base.join("big.bin");
    std::fs::write(&big_source, vec![7u8; 9 * 1024 * 1024]).unwrap();
    let big = object_utils::ingest_file("big.bin", &big_source, DirEntryType::Normal, IngestMode::Store).unwrap();
    assert!(big.item_type.is_chunked(), "the source must be large enough to chunk");

    let mut writers: Vec<Writer> = vec![
        ("regular", Box::new(|path| shift_utils::write_tracked_file(path, &blob, DirEntryType::Normal, true))),
        ("chunked", Box::new(|path| shift_utils::write_tracked_file(path, &big.hash, big.item_type, true))),
        ("merged", Box::new(|path| shift_utils::write_working_file(path, b"merged\n", DirEntryType::Normal, true))),
    ];
    if cfg!(unix) {
        writers.push(("symlink", Box::new(|path| {
            shift_utils::write_tracked_file(path, &link_target, DirEntryType::SymbolicLink, true)
        })));
    }

    // The occupant under the exact name everywhere; under another spelling where the volume
    // folds case, which is the alias the up-front checks exist for.
    let folds = {
        std::fs::write("probe", "").unwrap();
        let folds = Path::new("PROBE").exists();
        std::fs::remove_file("probe").unwrap();
        folds
    };
    let mut spellings = vec!["readme"];
    if folds {
        spellings.push("README");
    }

    for (label, write) in &writers {
        for occupant in &spellings {
            for occupant_kind in ["file", "symlink"] {
                if occupant_kind == "symlink" && !cfg!(unix) {
                    continue;
                }
                let case = format!("{} write onto a {} at {}", label, occupant_kind, occupant);

                place_occupant(occupant, occupant_kind);
                let written = write("readme");

                let error = written.expect_err(&format!("{}: must fail", case));
                assert!(error.contains("already exists"), "{}: {}", case, error);
                assert_occupant_intact(occupant, occupant_kind, &case);
                assert_eq!(listed_names(), vec![".forklift".to_string(), occupant.to_string()],
                           "{}: nothing else is left behind", case);

                std::fs::remove_file(occupant).unwrap();
            }
        }

        // With nothing there, the same write lands.
        write("readme").unwrap_or_else(|e| panic!("{} write onto nothing: {}", label, e));
        std::fs::remove_file("readme").unwrap();
    }

    std::env::set_current_dir(&base).unwrap();
    drop(_scope);
    let _ = std::fs::remove_dir_all(&base);
}

fn store_blob(content: &[u8]) -> String {
    let mut object = LooseObjectBuilder::build_blob(&Blob { content: content.to_vec() });
    object.store().unwrap();
    object.hash
}

fn place_occupant(name: &str, kind: &str) {
    match kind {
        "file" => std::fs::write(name, "occupant\n").unwrap(),
        #[cfg(unix)]
        "symlink" => std::os::unix::fs::symlink("occupant-target", name).unwrap(),
        _ => unreachable!(),
    }
}

fn assert_occupant_intact(name: &str, kind: &str, case: &str) {
    let metadata = std::fs::symlink_metadata(name).unwrap();
    match kind {
        "file" => {
            assert!(metadata.file_type().is_file(), "{}: still a file", case);
            assert_eq!(std::fs::read_to_string(name).unwrap(), "occupant\n", "{}: content kept", case);
        }
        _ => {
            assert!(metadata.file_type().is_symlink(), "{}: still a symlink", case);
            assert_eq!(std::fs::read_link(name).unwrap(), Path::new("occupant-target"), "{}: target kept", case);
        }
    }
}

fn listed_names() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(".").unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

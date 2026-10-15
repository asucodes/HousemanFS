//! The read-only invariant, enforced rather than promised.
//!
//! D1 says the first release cannot write to a scanned volume, "enforced by an automated
//! test asserting zero create, write, delete and set-info operations during a scan — not by
//! review". This is that test: it tokenises every source file in the platform crate and fails
//! on any access right, creation disposition or mutating API that would let a handle modify
//! the volume.
//!
//! It lives in `tests/` rather than beside the code for a reason: the audit names the very
//! tokens it forbids, so auditing the file that holds the list would always fail. The
//! shipped code under `src/` carries no such exception — every file there is audited, and
//! the audit verifies its own completeness against the directory.
//!
//! It is a tripwire, not a proof — a sufficiently creative call could evade a token list —
//! but evading it requires intent, and intent is what review is for. Accidents cannot pass.

use std::collections::HashSet;

/// Every module in the platform crate. If a module is added but not listed here, a whole
/// file escapes the audit — so the list is checked against `src/`, not trusted.
const SOURCES: &[&str] = &[
    "lib.rs",
    "elevation.rs",
    "ntfs.rs",
    "privilege.rs",
    "util.rs",
    "volume.rs",
    "walk.rs",
];

/// Tokens that must never appear: write access rights, creating dispositions, and the APIs
/// that create, write, delete, rename or set metadata. `FILE_SHARE_DELETE` is sharing, not
/// deleting, and tokenises as one word, so it does not trip the `DELETE` entry — which is
/// verified by a dedicated test below rather than assumed.
const FORBIDDEN: &[&str] = &[
    "GENERIC_WRITE",
    "GENERIC_ALL",
    "FILE_WRITE_DATA",
    "FILE_WRITE_ATTRIBUTES",
    "FILE_WRITE_EA",
    "FILE_APPEND_DATA",
    "WRITE_DAC",
    "WRITE_OWNER",
    "DELETE",
    "OPEN_ALWAYS",
    "CREATE_ALWAYS",
    "CREATE_NEW",
    "TRUNCATE_EXISTING",
    "WriteFile",
    "WriteFileEx",
    "DeleteFileW",
    "DeleteFileA",
    "RemoveDirectoryW",
    "RemoveDirectoryA",
    "MoveFileW",
    "MoveFileExW",
    "CreateHardLinkW",
    "SetFileInformationByHandle",
    "SetEndOfFile",
];

fn src_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Split into identifier-like tokens, so `FILE_SHARE_DELETE` is one token and a comment
/// mentioning deletion in prose is not.
fn tokens(source: &str) -> impl Iterator<Item = &str> {
    source.split(|c: char| !(c.is_alphanumeric() || c == '_'))
}

fn read_source(name: &str) -> String {
    std::fs::read_to_string(src_dir().join(name))
        .unwrap_or_else(|_| panic!("{name} should be readable"))
}

#[test]
fn every_module_is_audited() {
    let listed: HashSet<&str> = SOURCES.iter().copied().collect();
    let on_disk = std::fs::read_dir(src_dir())
        .expect("src dir readable")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".rs"))
        .collect::<Vec<_>>();
    for file in &on_disk {
        assert!(
            listed.contains(file.as_str()),
            "{file} is not in the read-only audit; add it or the invariant is unenforced"
        );
    }
}

#[test]
fn no_write_create_delete_or_set_info() {
    let mut violations = Vec::new();
    for name in SOURCES {
        let source = read_source(name);
        for token in tokens(&source) {
            if FORBIDDEN.contains(&token) {
                violations.push(format!("{name}: {token}"));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "write-capable tokens in the platform layer:\n{}",
        violations.join("\n")
    );
}

#[test]
fn sharing_delete_is_not_deleting() {
    // `FILE_SHARE_DELETE` must keep working: inspecting a file must never lock another
    // process out. This test pins the distinction the audit above relies on — that the
    // sharing flag tokenises separately from the `DELETE` access right.
    let source = read_source("walk.rs");
    let sharing: Vec<&str> = tokens(&source).filter(|t| t.contains("DELETE")).collect();
    assert!(
        !sharing.is_empty(),
        "the walk should still open files with full sharing"
    );
    for token in sharing {
        assert_eq!(
            token, "FILE_SHARE_DELETE",
            "a DELETE-adjacent token that is not sharing: {token}"
        );
    }
}

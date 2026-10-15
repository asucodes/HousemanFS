//! Small pure helpers shared by every crate: identity, accounting, provenance.
//!
//! Anything here must be computable from values already in hand, with no operating-system
//! calls and no persistence. That is what keeps the decision-model boundary testable
//! anywhere without dragging in platform or storage code.

/// Whether a path refers to a whole volume, e.g. `C:` or `D:\`.
///
/// Volume-only operations (reconciliation, the dashboard's residual) hinge on this, so
/// every frontend answers it identically.
pub fn is_volume_root(path: &str) -> bool {
    let trimmed = path.trim_end_matches(['\\', '/']);
    let bytes = trimmed.as_bytes();
    bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// Split a filename into its extension, lowercased, without the dot.
///
/// Returns an empty string for names with no extension. Leading dots are not treated as
/// extensions, so `.gitignore` has none — which is what a user would expect.
pub fn extension_of(name: &str) -> String {
    let stem = name.rsplit(['\\', '/']).next().unwrap_or(name);
    match stem.rsplit_once('.') {
        Some((prefix, ext)) if !prefix.is_empty() && !ext.is_empty() => ext.to_ascii_lowercase(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_extraction() {
        assert_eq!(extension_of("report.pdf"), "pdf");
        assert_eq!(extension_of("archive.tar.gz"), "gz");
        assert_eq!(extension_of(".gitignore"), "");
        assert_eq!(extension_of("Makefile"), "");
        assert_eq!(extension_of("Report.PDF"), "pdf");
    }

    #[test]
    fn volume_roots_are_two_letter_drives() {
        assert!(is_volume_root("C:"));
        assert!(is_volume_root(r"C:\"));
        assert!(is_volume_root("D:/"));
        assert!(!is_volume_root(r"C:\Windows"));
        assert!(!is_volume_root(r"\\server\share"));
        assert!(!is_volume_root(""));
    }
}

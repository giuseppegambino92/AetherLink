//! Lexical path validation shared by the VFS host, the NFS adapter and the transfer receiver.

/// Split a share-relative, `/`-separated path into its components.
///
/// Rejects (with a human-readable reason): NUL bytes, absolute paths (`/x`, `\x`),
/// UNC prefixes (`\\server`, `//server`), Windows drive prefixes (`C:`), `..` components,
/// and — on Windows hosts — backslashes, `:` (alternate data streams) and reserved
/// device names. Empty components and `.` are ignored, so `""` and `"."` denote the root.
pub fn split_rel_path(p: &str) -> Result<Vec<&str>, String> {
    if p.contains('\0') {
        return Err("path contains a NUL byte".into());
    }
    if p.starts_with('/') || p.starts_with('\\') {
        return Err(format!("absolute or UNC path not allowed: {p:?}"));
    }
    if has_drive_prefix(p) {
        return Err(format!("drive-prefixed path not allowed: {p:?}"));
    }
    if cfg!(windows) && p.contains('\\') {
        return Err(format!("backslash not allowed in path: {p:?}"));
    }
    let mut out = Vec::new();
    for c in p.split('/') {
        match c {
            "" | "." => continue,
            ".." => return Err(format!("parent-directory component not allowed: {p:?}")),
            c => {
                validate_name(c)?;
                out.push(c);
            }
        }
    }
    Ok(out)
}

/// Validate a single path component (file name).
pub fn validate_name(c: &str) -> Result<(), String> {
    if c.is_empty() || c == "." || c == ".." {
        return Err(format!("invalid file name: {c:?}"));
    }
    if c.contains('/') || c.contains('\0') {
        return Err(format!("invalid character in file name: {c:?}"));
    }
    if c.len() > 255 {
        return Err("file name too long".into());
    }
    if cfg!(windows) {
        if c.contains(['\\', ':', '<', '>', '"', '|', '?', '*']) {
            return Err(format!("invalid character in file name: {c:?}"));
        }
        let stem = c
            .split('.')
            .next()
            .unwrap_or("")
            .trim_end()
            .to_ascii_uppercase();
        const RESERVED: [&str; 22] = [
            "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
            "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
        ];
        if RESERVED.contains(&stem.as_str()) {
            return Err(format!("reserved device name: {c:?}"));
        }
    }
    Ok(())
}

fn has_drive_prefix(p: &str) -> bool {
    let b = p.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// Lexically check that `target` (a symlink target) stays inside the share when
/// resolved relative to the directory `parent` (share-relative components).
pub(crate) fn symlink_target_is_contained(parent: &[&str], target: &str) -> Result<(), String> {
    if target.is_empty() {
        return Err("empty symlink target".into());
    }
    if target.contains('\0') {
        return Err("symlink target contains a NUL byte".into());
    }
    if target.starts_with('/') || target.starts_with('\\') || has_drive_prefix(target) {
        return Err("absolute symlink targets are not allowed".into());
    }
    if cfg!(windows) && target.contains('\\') {
        return Err("backslash not allowed in symlink target".into());
    }
    let mut depth = parent.len();
    for c in target.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                if depth == 0 {
                    return Err("symlink target escapes the share".into());
                }
                depth -= 1;
            }
            _ => depth += 1,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_normal_paths() {
        assert_eq!(split_rel_path("").unwrap(), Vec::<&str>::new());
        assert_eq!(split_rel_path(".").unwrap(), Vec::<&str>::new());
        assert_eq!(
            split_rel_path("a/b//c/./d").unwrap(),
            vec!["a", "b", "c", "d"]
        );
        assert_eq!(split_rel_path("dir/").unwrap(), vec!["dir"]);
        assert_eq!(
            split_rel_path("..foo/bar..").unwrap(),
            vec!["..foo", "bar.."]
        );
    }

    #[test]
    fn rejects_hostile_paths() {
        for bad in [
            "/etc/passwd",
            "\\\\server\\share",
            "//server/share",
            "C:/Windows",
            "c:foo",
            "..",
            "a/../../b",
            "a/..",
            "a\0b",
        ] {
            assert!(split_rel_path(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn symlink_targets() {
        assert!(symlink_target_is_contained(&["a", "b"], "../c").is_ok());
        assert!(symlink_target_is_contained(&["a"], "../c").is_ok());
        assert!(symlink_target_is_contained(&[], "x/../y").is_ok());
        assert!(symlink_target_is_contained(&[], "../c").is_err());
        assert!(symlink_target_is_contained(&["a"], "../../c").is_err());
        assert!(symlink_target_is_contained(&["a"], "/etc").is_err());
        assert!(symlink_target_is_contained(&["a"], "").is_err());
    }
}

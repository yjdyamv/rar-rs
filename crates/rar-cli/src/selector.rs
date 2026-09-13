//! Shared archive-member selection for the `rar` and `unrar` binaries.

/// Wildcard mask match: `*` matches any sequence (including `/`), `?`
/// matches a single character, everything else is literal. WinRAR folds
/// ASCII case in masks, so matching is case-insensitive on Windows; on
/// Unix the stored names are case-sensitive and matching stays exact.
pub fn mask_match(mask: &str, name: &str) -> bool {
    fn char_eq(mask: char, name: char) -> bool {
        if cfg!(windows) {
            mask.eq_ignore_ascii_case(&name)
        } else {
            mask == name
        }
    }
    let m: Vec<char> = mask.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let mut prev = vec![false; n.len() + 1];
    prev[0] = true;
    for mc in &m {
        let mut cur = vec![false; n.len() + 1];
        for (i, nc) in n.iter().enumerate() {
            let take = match mc {
                '*' => prev[i] || cur[i],
                '?' => prev[i],
                c => char_eq(*c, *nc) && prev[i],
            };
            cur[i + 1] = take;
        }
        if *mc == '*' {
            cur[0] = prev[0];
        }
        prev = cur;
    }
    prev[n.len()]
}

/// Match an archive member by its stored path, basename, `*`/`?` mask or
/// directory name.
///
/// A selector containing a path must match the complete stored path; a
/// selector naming a directory (without wildcards) also selects the whole
/// `dir/...` subtree, like the official extractor. Masks use the shared
/// [`mask_match`] policy (ASCII case is folded on Windows).
pub fn name_matches(member: &str, requested: &str) -> bool {
    let member = member.replace('\\', "/");
    let requested = requested.replace('\\', "/");
    let requested = requested.trim_end_matches('/');
    if requested.is_empty() {
        return false;
    }
    if requested.contains(['*', '?']) {
        return mask_match(requested, &member)
            || (!requested.contains('/')
                && member
                    .rsplit('/')
                    .next()
                    .is_some_and(|name| mask_match(requested, name)));
    }
    if eq_name(&member, requested) || under_dir(&member, requested) {
        return true;
    }
    !requested.contains('/')
        && member
            .rsplit('/')
            .next()
            .is_some_and(|name| eq_name(name, requested))
}

/// Whether two name components are equal under the platform's case rules
/// (Windows folds ASCII case, matching WinRAR; Unix stays exact).
fn eq_name(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// Whether `member` lives under the directory `requested` (separator-aware,
/// so `sub` never matches `submarine/...`).
fn under_dir(member: &str, requested: &str) -> bool {
    let requested = requested.trim_end_matches('/');
    let bytes = requested.as_bytes();
    member
        .as_bytes()
        .get(..bytes.len() + 1)
        .is_some_and(|prefix| {
            prefix[bytes.len()] == b'/' && {
                if cfg!(windows) {
                    prefix[..bytes.len()].eq_ignore_ascii_case(bytes)
                } else {
                    prefix[..bytes.len()] == *bytes
                }
            }
        })
}

/// Select member identities in archive order. An empty selector list selects
/// all members. Distinct identities are preserved when names are duplicated.
pub fn select_entries<'a, T>(
    members: impl IntoIterator<Item = (T, &'a str)>,
    requested: &[String],
) -> Vec<T> {
    members
        .into_iter()
        .filter(|(_, member)| {
            requested.is_empty()
                || requested
                    .iter()
                    .any(|selector| name_matches(member, selector))
        })
        .map(|(id, _)| id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{name_matches, select_entries};

    #[test]
    fn does_not_match_member_as_selector_suffix() {
        assert!(!name_matches("a", "data"));
        assert!(!name_matches("data", "a"));
    }

    #[test]
    fn matches_basename() {
        assert!(name_matches("dir/sub/file.txt", "file.txt"));
    }

    #[test]
    fn path_selector_requires_the_full_stored_path() {
        assert!(name_matches("dir/sub/file.txt", "dir/sub/file.txt"));
        assert!(!name_matches("top/dir/sub/file.txt", "dir/sub/file.txt"));
        assert!(!name_matches("dir/sub/file.txt", "sub/file.txt"));
    }

    #[test]
    fn normalizes_backslashes_before_matching() {
        assert!(name_matches("dir\\sub\\file.txt", "dir/sub/file.txt"));
        assert!(name_matches("dir/sub/file.txt", "dir\\sub\\file.txt"));
        assert!(name_matches("dir\\sub\\file.txt", "file.txt"));
    }

    #[test]
    fn directory_selectors_match_the_subtree() {
        assert!(name_matches("sub", "sub"));
        assert!(name_matches("sub/f3.txt", "sub"));
        assert!(name_matches("sub/deep/f4.bin", "sub"));
        assert!(name_matches("sub/f3.txt", "sub/"));
        assert!(!name_matches("submarine/f.txt", "sub"));
        assert!(!name_matches("top/sub/f.txt", "sub"));
    }

    #[test]
    fn wildcard_selectors_match_like_masks() {
        assert!(name_matches("a.txt", "*.txt"));
        assert!(name_matches("sub/f3.txt", "*.txt"));
        assert!(name_matches("sub/f3.txt", "sub/*.txt"));
        assert!(!name_matches("other/sub/f3.txt", "sub/*.txt"));
        assert!(!name_matches("a.bin", "*.txt"));
        assert!(name_matches("big.txt", "big?txt"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_folds_selector_case() {
        assert!(name_matches("Dir/File.TXT", "dir"));
        assert!(name_matches("Dir/File.TXT", "FILE.txt"));
        assert!(name_matches("Dir/File.TXT", "*.txt"));
    }

    #[test]
    fn empty_selector_list_selects_every_member_in_order() {
        let members = [(1, "a"), (2, "dir/b")];
        assert_eq!(select_entries(members, &[]), [1, 2]);
    }

    #[test]
    fn duplicate_names_keep_their_distinct_identities() {
        let members = [(1, "same.bin"), (2, "same.bin"), (3, "other.bin")];
        assert_eq!(select_entries(members, &["same.bin".to_string()]), [1, 2]);
    }

    #[test]
    fn directory_selector_selects_its_files() {
        let members = [(1, "sub"), (2, "sub/f.txt"), (3, "other.txt")];
        assert_eq!(select_entries(members, &["sub".to_string()]), [1, 2]);
    }
}

//! Shared archive-member selection for the `rar` and `unrar` binaries.

/// Match an archive member by its exact stored path or by basename.
///
/// A selector containing a path must match the complete stored path. This
/// avoids treating arbitrary suffixes or prefixes as member names.
pub fn name_matches(member: &str, requested: &str) -> bool {
    let member = member.replace('\\', "/");
    let requested = requested.replace('\\', "/");
    member == requested
        || (!requested.contains('/')
            && member
                .rsplit('/')
                .next()
                .is_some_and(|name| name == requested))
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
    fn empty_selector_list_selects_every_member_in_order() {
        let members = [(1, "a"), (2, "dir/b")];
        assert_eq!(select_entries(members, &[]), [1, 2]);
    }

    #[test]
    fn duplicate_names_keep_their_distinct_identities() {
        let members = [(1, "same.bin"), (2, "same.bin"), (3, "other.bin")];
        assert_eq!(select_entries(members, &["same.bin".to_string()]), [1, 2]);
    }
}

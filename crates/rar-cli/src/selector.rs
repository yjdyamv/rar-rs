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

/// Select members in archive order. An empty selector list selects all
/// members.
pub fn select_members<'a>(
    members: impl IntoIterator<Item = &'a str>,
    requested: &[String],
) -> Vec<&'a str> {
    members
        .into_iter()
        .filter(|member| {
            requested.is_empty()
                || requested
                    .iter()
                    .any(|selector| name_matches(member, selector))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{name_matches, select_members};

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
        let members = ["a", "dir/b"];
        assert_eq!(select_members(members, &[]), members);
    }
}

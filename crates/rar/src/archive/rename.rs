//! Rename-map construction shared by the RAR4 and RAR5 edit engines.

use std::collections::HashMap;

use super::ArchiveEntry;
use crate::error::{RarError, RarResult};

/// Build the `index -> new name` map for a set of rename pairs, expanding a
/// renamed directory to all of its descendants. Names are normalized
/// (trailing slashes trimmed; a renamed directory keeps one trailing slash).
/// A stale index fails with [`RarError::StaleEntryId`]. Returns the map and
/// the number of explicit (non-expanded) renames.
pub(crate) fn build_rename_map(
    entries: &[ArchiveEntry],
    renames: &[(usize, String)],
) -> RarResult<(HashMap<usize, String>, usize)> {
    let mut map: HashMap<usize, String> = HashMap::new();
    let mut count = 0usize;
    for (idx, new) in renames {
        if *idx >= entries.len() {
            return Err(RarError::StaleEntryId);
        }
        let old_norm = map
            .get(idx)
            .map(|n| n.as_str())
            .unwrap_or(entries[*idx].name())
            .trim_end_matches('/')
            .to_string();
        let is_dir = entries[*idx].is_dir();
        let new_norm = new.trim_end_matches('/').to_string();
        if is_dir {
            map.insert(*idx, format!("{new_norm}/"));
            let prefix = format!("{old_norm}/");
            for (i, e) in entries.iter().enumerate() {
                if i == *idx || map.contains_key(&i) {
                    continue;
                }
                if let Some(rest) = e.name().strip_prefix(&prefix) {
                    map.insert(i, format!("{new_norm}/{rest}"));
                }
            }
        } else {
            map.insert(*idx, new_norm.clone());
        }
        count += 1;
    }
    Ok((map, count))
}

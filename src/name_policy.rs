//! Name and filter policy for adding files — the `-ep` / `-ep1` / `-ap` /
//! `-x` / `-n` / `-cl` / `-cu` / `-r-` semantics of the `rar a` command.
//!
//! The CLI collects its arguments through [`collect`] and hands the result
//! to [`crate::RarArchive::add_batch`]; library users and tests can drive
//! the same policy directly instead of through a subprocess.
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Case conversion applied to stored names (`-cl` / `-cu`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CaseKind {
    Lower,
    Upper,
}

/// Name and filter policy for the `a` command (`-ep`, `-ep1`, `-ap`,
/// `-x`, `-n`, `-cl`, `-cu`, `-r-`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NamePolicy {
    pub path_prefix: Option<String>,
    /// Store basename-only names, no directory entries (`-ep`).
    pub basename_only: bool,
    /// Exclude the base directory from names (`-ep1`, wildcard paths).
    pub strip_base: bool,
    /// Do not recurse into directories (`-r-`).
    pub no_recurse: bool,
    pub case: Option<CaseKind>,
    pub include_masks: Vec<String>,
    pub exclude_masks: Vec<String>,
}

impl NamePolicy {
    /// Whether a file with the relative archive name should be added
    /// (masks match the relative name, like `rar -x`/`-n`).
    fn file_kept(&self, rel: &str) -> bool {
        let included =
            self.include_masks.is_empty() || self.include_masks.iter().any(|m| mask_match(m, rel));
        let excluded = self.exclude_masks.iter().any(|m| mask_match(m, rel));
        included && !excluded
    }

    /// Whether a directory entry should be written (dirs are skipped with
    /// `-ep` and when include masks are present, like `rar -n`).
    fn dir_entry_kept(&self, rel: &str) -> bool {
        !self.basename_only
            && self.include_masks.is_empty()
            && !self.exclude_masks.iter().any(|m| mask_match(m, rel))
    }

    /// Whether the whole directory subtree should be skipped.
    fn dir_subtree_skipped(&self, rel: &str) -> bool {
        self.exclude_masks.iter().any(|m| mask_match(m, rel))
    }

    /// The stored archive name for a member with the relative name `rel`,
    /// applying `-ep`, `-ep1` and `-ap` in that order.
    fn stored_name(&self, rel: &str) -> String {
        let mut name = rel.to_string();
        if self.strip_base
            && let Some((_, rest)) = name.split_once('/')
            && !rest.is_empty()
        {
            name = rest.to_string();
        }
        if self.basename_only {
            name = name.rsplit('/').next().unwrap_or(&name).to_string();
        }
        if let Some(kind) = self.case {
            name = match kind {
                CaseKind::Lower => name.to_lowercase(),
                CaseKind::Upper => name.to_uppercase(),
            };
        }
        match &self.path_prefix {
            Some(prefix) => format!("{prefix}/{name}"),
            None => name,
        }
    }
}

/// One collected add target, ready to convert into a [`crate::BatchEntry`].
#[derive(Debug)]
pub struct Collected {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub level: u8,
}

/// Normalize a path argument into an archive name: relative paths stay as
/// given, absolute paths drop the leading slash (like `rar`).
pub fn arg_to_name(arg: &str) -> String {
    arg.trim_start_matches('/')
        .trim_end_matches('/')
        .replace('\\', "/")
}

fn has_wildcards(arg: &str) -> bool {
    arg.contains('*') || arg.contains('?')
}

/// Collect the members for the given arguments under `policy`, expanding
/// wildcards and walking directories (like the official `rar a`).
pub fn collect(policy: &NamePolicy, args: &[String], level: u8) -> Result<Vec<Collected>, String> {
    let mut pending: Vec<Collected> = Vec::new();
    let mut added: HashSet<String> = HashSet::new();
    for arg in args {
        add_with_policy(&mut pending, arg, level, policy, &mut added)
            .map_err(|e| format!("add {arg}: {e}"))?;
    }
    Ok(pending)
}

/// Add a file or directory tree honoring the name/filter policy.
fn add_with_policy(
    pending: &mut Vec<Collected>,
    arg: &str,
    level: u8,
    policy: &NamePolicy,
    added: &mut HashSet<String>,
) -> Result<(), String> {
    if has_wildcards(arg) {
        return add_wildcard_arg(pending, arg, level, policy, added);
    }
    let path = Path::new(arg);
    if !path.exists() {
        return Err(format!("path not found: {arg}"));
    }
    if path.is_file() {
        // Relative path names, matching the official `rar a`; `-ep1`
        // strips the parent directories (like the official tool).
        let rel = arg_to_name(arg);
        let name = if policy.basename_only || policy.strip_base {
            let basename = rel.rsplit('/').next().unwrap_or(&rel).to_string();
            match &policy.path_prefix {
                Some(prefix) => format!("{prefix}/{basename}"),
                None => basename,
            }
        } else {
            policy.stored_name(&rel)
        };
        if policy.file_kept(&rel) && added.insert(name.clone()) {
            pending.push(Collected {
                path: path.to_path_buf(),
                name,
                is_dir: false,
                level,
            });
        }
        return Ok(());
    }
    // `-ep1` is ignored for plain directory arguments (the official tool
    // applies it to wildcard paths only).
    let plain = NamePolicy {
        path_prefix: policy.path_prefix.clone(),
        basename_only: policy.basename_only,
        strip_base: false,
        no_recurse: policy.no_recurse,
        case: policy.case,
        include_masks: policy.include_masks.clone(),
        exclude_masks: policy.exclude_masks.clone(),
    };
    let rel = arg_to_name(arg);
    if plain.dir_subtree_skipped(&rel) {
        return Ok(());
    }
    if plain.dir_entry_kept(&rel) {
        pending.push(Collected {
            path: path.to_path_buf(),
            name: plain.stored_name(&rel),
            is_dir: true,
            level,
        });
    }
    if plain.no_recurse {
        return Ok(());
    }
    walk_directory(pending, path, &rel, level, &plain, added)
}

/// Expand a wildcard argument (`sub/*.txt`, like the official `rar`, which
/// performs its own pattern expansion). `-ep1` drops the pattern's base
/// directory from the stored names.
fn add_wildcard_arg(
    pending: &mut Vec<Collected>,
    pattern: &str,
    level: u8,
    policy: &NamePolicy,
    added: &mut HashSet<String>,
) -> Result<(), String> {
    let wc = pattern.find(['*', '?']).unwrap();
    let prefix = &pattern[..wc];
    let base_dir = match prefix.rfind('/') {
        Some(i) => &prefix[..i],
        None => ".",
    };
    if base_dir.is_empty() {
        return Ok(());
    }
    let base_path = Path::new(base_dir);
    if !base_path.is_dir() {
        return Ok(());
    }
    let rel_base = arg_to_name(base_dir);
    let mut children: Vec<_> = std::fs::read_dir(base_path)
        .map_err(|e| format!("read dir {}: {e}", base_path.display()))?
        .filter_map(|e| e.ok())
        .collect();
    children.sort_by_key(|e| e.file_name());
    for child in children {
        let rel = if rel_base.is_empty() {
            child.file_name().to_string_lossy().into_owned()
        } else {
            format!("{rel_base}/{}", child.file_name().to_string_lossy())
        };
        if !mask_match(pattern, &rel) {
            continue;
        }
        if child.path().is_dir() {
            if policy.dir_subtree_skipped(&rel) {
                continue;
            }
            if policy.dir_entry_kept(&rel) {
                pending.push(Collected {
                    path: child.path(),
                    name: policy.stored_name(&rel),
                    is_dir: true,
                    level,
                });
            }
            if !policy.no_recurse {
                walk_directory(pending, &child.path(), &rel, level, policy, added)?;
            }
        } else if policy.file_kept(&rel) && added.insert(policy.stored_name(&rel)) {
            pending.push(Collected {
                path: child.path(),
                name: policy.stored_name(&rel),
                is_dir: false,
                level,
            });
        }
    }
    Ok(())
}

fn walk_directory(
    pending: &mut Vec<Collected>,
    dir: &Path,
    rel_dir: &str,
    level: u8,
    policy: &NamePolicy,
    added: &mut HashSet<String>,
) -> Result<(), String> {
    let mut children: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| format!("read dir {}: {e}", dir.display()))?
        .filter_map(|e| e.ok())
        .collect();
    children.sort_by_key(|e| e.file_name());
    for child in children {
        let rel = format!("{rel_dir}/{}", child.file_name().to_string_lossy());
        if child.path().is_dir() {
            if policy.dir_subtree_skipped(&rel) {
                continue;
            }
            if policy.dir_entry_kept(&rel) {
                pending.push(Collected {
                    path: child.path(),
                    name: policy.stored_name(&rel),
                    is_dir: true,
                    level,
                });
            }
            if !policy.no_recurse {
                walk_directory(pending, &child.path(), &rel, level, policy, added)?;
            }
        } else if policy.file_kept(&rel) && added.insert(policy.stored_name(&rel)) {
            pending.push(Collected {
                path: child.path(),
                name: policy.stored_name(&rel),
                is_dir: false,
                level,
            });
        }
    }
    Ok(())
}

/// Wildcard mask match: `*` matches any sequence (including `/`), `?`
/// matches a single character, everything else is literal; matching is
/// case-sensitive (like `rar -x`/`-n` on Unix).
pub fn mask_match(mask: &str, name: &str) -> bool {
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
                c => *nc == *c && prev[i],
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_match_star_question_literal() {
        assert!(mask_match("*.txt", "a/b.txt"));
        assert!(mask_match("sub/*", "sub/f3.txt"));
        assert!(!mask_match("*.txt", "a.bin"));
        assert!(mask_match("a?c", "abc"));
        assert!(!mask_match("a?c", "abdc"));
        assert!(mask_match("*", "anything/at/all"));
    }

    #[test]
    fn collect_applies_exclude_and_basename_policy() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("f1.txt"), b"1").unwrap();
        std::fs::write(dir.path().join("f2.tmp"), b"2").unwrap();
        std::fs::write(dir.path().join("sub").join("f3.txt"), b"3").unwrap();

        let cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        let result = (|| -> Result<Vec<Collected>, String> {
            let args: Vec<String> = vec!["sub".into()];
            let policy = NamePolicy {
                exclude_masks: vec!["*.tmp".into()],
                ..Default::default()
            };
            collect(&policy, &args, 3)
        })();
        std::env::set_current_dir(cwd).unwrap();
        let collected = result.unwrap();
        let names: Vec<String> = collected.iter().map(|c| c.name.clone()).collect();
        assert_eq!(names, ["sub", "sub/f3.txt"], "exclude mask must apply");

        let cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        let flat = (|| -> Result<Vec<Collected>, String> {
            let args: Vec<String> = vec!["sub/f3.txt".into()];
            let policy = NamePolicy {
                basename_only: true,
                ..Default::default()
            };
            collect(&policy, &args, 3)
        })();
        std::env::set_current_dir(cwd).unwrap();
        let flat = flat.unwrap();
        assert_eq!(flat.len(), 1);
        assert_eq!(flat[0].name, "f3.txt", "-ep must store the basename");
    }
}

//! Legacy RAR 1.5–4.x recovery volumes (`.rev` files).
//!
//! A legacy `.rev` file is raw Reed-Solomon parity over the volume set: for
//! every byte offset, the bytes of all data volumes form one GF(2^8) RS
//! codeword (see [`rs8`]), and each recovery volume stores one parity
//! symbol per offset. Two on-disk layouts exist, both verified byte-for-byte
//! against WinRAR 7.23 (`rv`/`rc`):
//!
//! - **trailer format** (RAR 4.20+, used when the volumes end in zero bytes,
//!   as WinRAR's 20-byte `ENDARC` guarantees): the `.rev` length equals the
//!   largest volume; the last 7 bytes are
//!   `[data_count - 1, recovery_count - 1, recovery_index, CRC32]` over the
//!   preceding bytes plus the first three trailer bytes, and the parity
//!   protects offsets `0..len - 7` only. A rebuilt volume's last 7 bytes
//!   are zeros, exactly like WinRAR. Names: `{base}.part{NN}.rev` for
//!   `.partN.rar` sets, `{base}{N}.rev` for `.rar`/`.rNN` sets.
//! - **legacy format** (RAR 3.0-era volumes without zero tails): the whole
//!   file is parity and the counts live in the file name,
//!   `{base}<data_count>_<recovery_count>_<index + 1>.rev` (new-naming sets
//!   keep their part infix: `{base}.part<data>_<rec>_<index>.rev`).
//!
//! Repair (`rc`) accepts both layouts, locates damaged volumes with the RS
//! syndromes (Berlekamp-Massey, up to `floor(recovery_count / 2)` unknown
//! damaged volumes), renames damaged data volumes to `*.bad` and writes the
//! rebuilt volumes in their place, mirroring WinRAR.
//!
//! The RS codec is ported from `rars`' `recovery/rar3.rs` (MIT OR
//! Apache-2.0 per the rars workspace metadata — see NOTICE); the on-disk
//! layouts were reverse-engineered from WinRAR output.

pub(crate) mod rs8;

mod build;
mod layout;
mod name;
mod repair;
mod trailer;

pub(crate) use build::build_recovery_volumes_for_set;
pub(crate) use name::{canonical_recovery_names, is_legacy_rev_set, rev_name_belongs_to_set};
pub(crate) use repair::rebuild_missing_volumes;

use crate::error::RarError;

pub(super) fn map_coder(error: rs8::Rs8Error) -> RarError {
    RarError::Format(format!("legacy recovery: {error}"))
}

#[cfg(test)]
mod tests {
    use super::RarError;
    use super::build::{build_recovery_volumes_for_set_chunked, commit_rebuilt_volumes};
    use super::layout::{collect_recovery_volumes, identify};
    use super::name::{
        NameKind, part_width_candidates, rev_name_belongs_to_set, rev_name_candidates,
        trailer_style,
    };
    use super::repair::rebuild_missing_volumes_chunked;
    use super::rs8::Rsc8;
    use super::trailer::{Meta, TRAILER_LEN, parse_trailer, parse_trailer_file, write_trailer};
    use crate::fs::volume::volume_path_rar4;
    use std::path::{Path, PathBuf};

    #[test]
    fn trailer_roundtrip_validates_counts_and_crc() {
        let meta = Meta {
            data_count: 4,
            rec_count: 2,
            recovery_index: 1,
        };
        let payload = b"parity payload";
        let mut file = payload.to_vec();
        write_trailer(&meta, payload, &mut file);
        assert_eq!(parse_trailer(&file), Some(meta));
        let mut corrupt = file.clone();
        corrupt[0] ^= 0xff;
        assert_eq!(parse_trailer(&corrupt), None);
    }

    #[test]
    fn rev_names_cover_all_four_shapes() {
        let cases = [
            ("s.part1.rev", NameKind::NewTrailer, "s", true, None),
            ("o1.rev", NameKind::OldTrailer, "o", false, None),
            (
                "o4_2_1.rev",
                NameKind::OldLegacy,
                "o",
                false,
                Some(Meta {
                    data_count: 4,
                    rec_count: 2,
                    recovery_index: 0,
                }),
            ),
            (
                "rev_oldstyle.part4_2_2.rev",
                NameKind::NewLegacy,
                "rev_oldstyle",
                true,
                Some(Meta {
                    data_count: 4,
                    rec_count: 2,
                    recovery_index: 1,
                }),
            ),
        ];
        for (name, kind, base, new_naming, meta) in cases {
            let candidates = rev_name_candidates(name);
            let Some(parsed) = candidates
                .iter()
                .find(|candidate| candidate.base == base)
                .cloned()
            else {
                panic!("{name}: no candidate with base {base}: {candidates:?}");
            };
            assert_eq!(parsed.kind, kind, "{name}");
            assert_eq!(parsed.base, base, "{name}");
            assert_eq!(parsed.new_naming, new_naming, "{name}");
            assert_eq!(parsed.meta, meta, "{name}");
        }
        assert!(rev_name_candidates("plain.rar").is_empty());
        assert!(rev_name_candidates("data.bin").is_empty());
    }

    #[test]
    fn digit_ending_bases_offer_both_splits() {
        // `mv4` + `4_1_1` reads as `mv` + `44_1_1` too; both candidates
        // must be offered so the caller can pick by existing volumes.
        let candidates = rev_name_candidates("mv44_1_1.rev");
        let bases: Vec<&str> = candidates.iter().map(|c| c.base.as_str()).collect();
        assert!(bases.contains(&"mv4"), "{bases:?}");
        assert!(bases.contains(&"mv"), "{bases:?}");
        let data_counts: Vec<usize> = candidates
            .iter()
            .filter_map(|c| c.meta.map(|m| m.data_count))
            .collect();
        assert!(data_counts.contains(&4), "{data_counts:?}");
        assert!(data_counts.contains(&44), "{data_counts:?}");
    }

    /// Scanning a directory with a subdirectory and an unrelated file must
    /// not abort recovery (the subdirectory used to be read and fail).
    #[test]
    fn collect_recovery_volumes_ignores_non_rev_entries() {
        let dir = tempfile::tempdir().unwrap();
        // A legacy-format name carries its metadata, so no data volumes
        // need to exist for collection to succeed.
        std::fs::write(dir.path().join("set4_1_1.rev"), b"parity payload").unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        std::fs::write(dir.path().join("unrelated.bin"), vec![0u8; 4096]).unwrap();

        let set = collect_recovery_volumes(dir.path(), "set").unwrap();
        assert_eq!(set.meta.data_count, 4);
        assert_eq!(set.meta.rec_count, 1);
        assert_eq!(set.payloads.len(), 1);
        assert_eq!(set.payloads[0].len, 14);
        assert_eq!(set.payloads[0].path, dir.path().join("set4_1_1.rev"));
    }

    /// A same-base `.rev` describing a different (stale) set must be
    /// ignored, not abort recovery: the best-scoring metadata group wins.
    #[test]
    fn collect_recovery_volumes_skips_stray_same_base_metadata() {
        let dir = tempfile::tempdir().unwrap();
        // Three data volumes exist; the real set protects all three, the
        // stray same-base file protects only two.
        for index in 1..=3 {
            std::fs::write(
                dir.path().join(format!("set.part{index}.rar")),
                vec![0u8; 64],
            )
            .unwrap();
        }
        std::fs::write(dir.path().join("set.part3_1_1.rev"), b"real parity").unwrap();
        std::fs::write(dir.path().join("set.part2_1_1.rev"), b"stray parity").unwrap();

        let set = collect_recovery_volumes(dir.path(), "set").unwrap();
        assert_eq!(set.meta.data_count, 3);
        assert_eq!(set.meta.rec_count, 1);
        assert_eq!(set.payloads.len(), 1);
        assert_eq!(set.payloads[0].path, dir.path().join("set.part3_1_1.rev"));
    }

    #[test]
    fn part_width_candidates_cover_five_digit_sets() {
        assert_eq!(part_width_candidates(5), vec![5, 1, 2, 3, 4]);
        assert_eq!(part_width_candidates(2), vec![2, 1, 3, 4, 5]);
        assert_eq!(part_width_candidates(0), vec![1, 2, 3, 4, 5]);
    }

    /// `trailer_style` must accept a valid trailer, reject a CRC-corrupted
    /// one, and reject a legacy-format file, all through the file-backed
    /// streaming parser.
    #[test]
    fn trailer_style_detects_trailer_and_legacy_files() {
        let dir = tempfile::tempdir().unwrap();
        let meta = Meta {
            data_count: 4,
            rec_count: 2,
            recovery_index: 0,
        };
        let payload = vec![0x5au8; 4096];
        let mut trailer_file = payload.clone();
        write_trailer(&meta, &payload, &mut trailer_file);
        let trailer_path = dir.path().join("set.part1.rev");
        std::fs::write(&trailer_path, &trailer_file).unwrap();

        assert_eq!(
            parse_trailer_file(&trailer_path).unwrap(),
            parse_trailer(&trailer_file)
        );
        assert!(trailer_style(&trailer_path));

        // A flipped payload byte fails the streamed CRC check.
        let mut corrupt = trailer_file.clone();
        corrupt[0] ^= 0xff;
        let corrupt_path = dir.path().join("corrupt.part1.rev");
        std::fs::write(&corrupt_path, &corrupt).unwrap();
        assert!(!trailer_style(&corrupt_path));

        // A legacy-format file carries no trailer.
        let legacy_path = dir.path().join("set4_1_1.rev");
        std::fs::write(&legacy_path, b"parity payload").unwrap();
        assert!(!trailer_style(&legacy_path));
    }

    /// Deterministic non-archive byte patterns: the builder only reads the
    /// volume files, so plain files exercise the `.rev` codec directly.
    fn write_fake_volumes(dir: &Path, sizes: &[u64]) -> Vec<PathBuf> {
        write_fake_volumes_padded(dir, sizes, 1)
    }

    /// [`write_fake_volumes`] with an explicit part-number padding.
    fn write_fake_volumes_padded(dir: &Path, sizes: &[u64], padding: usize) -> Vec<PathBuf> {
        let mut volumes = Vec::with_capacity(sizes.len());
        for (i, &size) in sizes.iter().enumerate() {
            let path = dir.join(format!(
                "set.part{:0padding$}.rar",
                i + 1,
                padding = padding
            ));
            let mut bytes = vec![0u8; size as usize];
            let mut state = 0x1234_5678u32.wrapping_add(i as u32 + 1);
            for byte in &mut bytes {
                state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                *byte = (state >> 16) as u8;
            }
            std::fs::write(&path, &bytes).unwrap();
            volumes.push(path);
        }
        volumes
    }

    /// Staging temp names the builder may have leaked into `dir`.
    fn temp_leftovers(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("rar5tmp"))
            .collect()
    }

    /// Commit-transaction names (`rar5bak`/`rar5commit`) left in `dir`.
    fn commit_leftovers(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("rar5bak") || name.contains("rar5commit"))
            .collect()
    }

    /// Reference parity for the legacy (full-parity) layout, computed with
    /// full zero-padded volume shards in memory.
    fn legacy_reference(volumes: &[PathBuf], rec_count: usize) -> Vec<Vec<u8>> {
        let chunks: Vec<Vec<u8>> = volumes
            .iter()
            .map(|path| std::fs::read(path).unwrap())
            .collect();
        let shard_len = chunks.iter().map(Vec::len).max().unwrap();
        let coder = Rsc8::new(rec_count).unwrap();
        let mut payloads: Vec<Vec<u8>> = vec![Vec::with_capacity(shard_len); rec_count];
        let mut column = vec![0u8; chunks.len()];
        for offset in 0..shard_len {
            for (index, chunk) in chunks.iter().enumerate() {
                column[index] = chunk.get(offset).copied().unwrap_or(0);
            }
            for (payload, byte) in payloads.iter_mut().zip(coder.encode(&column)) {
                payload.push(byte);
            }
        }
        payloads
    }

    /// A 64-byte stripe over ~1 KiB volumes spans many stripes; the written
    /// parity must equal the buffered reference, and a missing volume must
    /// rebuild byte-identically through the streaming reader.
    #[test]
    fn streaming_build_and_rebuild_match_buffered_reference() {
        let dir = tempfile::tempdir().unwrap();
        let volumes = write_fake_volumes(dir.path(), &[1024, 1024, 700]);
        let expected = legacy_reference(&volumes, 8);

        let written = build_recovery_volumes_for_set_chunked(&volumes, 8, 64).unwrap();
        assert_eq!(written.len(), 8);
        for (k, path) in written.iter().enumerate() {
            assert_eq!(
                path.file_name().unwrap().to_string_lossy(),
                format!("set.part{}_{}_{}.rev", volumes.len(), 8, k + 1),
                "the builder must return the final paths"
            );
            let actual = std::fs::read(path).unwrap();
            assert_eq!(actual.len(), expected[k].len(), "rev {k} length");
            let diff = actual.iter().zip(&expected[k]).position(|(a, b)| a != b);
            assert_eq!(diff, None, "rev {k} first difference");
        }
        assert!(
            temp_leftovers(dir.path()).is_empty(),
            "successful build left temps: {:?}",
            temp_leftovers(dir.path())
        );

        // Only the last volume of a set may be short, so a full middle
        // volume is the reconstructable victim.
        let missing = std::fs::read(&volumes[1]).unwrap();
        std::fs::remove_file(&volumes[1]).unwrap();
        let rebuilt = rebuild_missing_volumes_chunked(&volumes[0], None, None, 64).unwrap();
        assert_eq!(rebuilt, vec![volumes[1].clone()]);
        assert_eq!(std::fs::read(&volumes[1]).unwrap(), missing);
    }

    /// A five-digit part width: a legacy `.rev` name carries no padding, so
    /// the data-volume probe must cover five digits and a reconstructed
    /// missing volume must keep the set's own padding.
    #[test]
    fn five_digit_legacy_set_rebuilds_with_padded_names() {
        let dir = tempfile::tempdir().unwrap();
        let volumes = write_fake_volumes_padded(dir.path(), &[1024, 1024, 700], 5);
        let original = std::fs::read(&volumes[1]).unwrap();
        let revs = build_recovery_volumes_for_set_chunked(&volumes, 2, 64).unwrap();
        // Plain files do not end in zeros: the builder picks the legacy
        // full-parity layout, whose name carries no part padding.
        assert_eq!(
            revs[0].file_name().unwrap().to_string_lossy(),
            "set.part3_2_1.rev"
        );

        std::fs::remove_file(&volumes[1]).unwrap();
        let rebuilt = rebuild_missing_volumes_chunked(&volumes[0], None, None, 64).unwrap();
        assert_eq!(rebuilt, vec![volumes[1].clone()]);
        assert_eq!(std::fs::read(&volumes[1]).unwrap(), original);
    }

    /// A failed streaming build removes the temps it wrote and leaves a
    /// pre-existing `.rev` untouched.
    #[test]
    fn streaming_build_failure_leaves_existing_revs_and_removes_temps() {
        let dir = tempfile::tempdir().unwrap();
        let volumes = write_fake_volumes(dir.path(), &[1024, 1024]);
        // A pre-existing `.rev` for the second output: the failed build
        // must leave it byte-identical.
        let existing = dir.path().join("set.part2_2_2.rev");
        let keep = b"pre-existing parity".to_vec();
        std::fs::write(&existing, &keep).unwrap();
        // Occupy the first final path with a directory so installing the
        // first completed temp fails after the whole parity set is built.
        std::fs::create_dir(dir.path().join("set.part2_2_1.rev")).unwrap();

        let error = build_recovery_volumes_for_set_chunked(&volumes, 2, 64).unwrap_err();
        assert!(matches!(error, RarError::Io(_)), "got {error}");
        assert_eq!(
            std::fs::read(&existing).unwrap(),
            keep,
            "the pre-existing .rev must survive the failed build"
        );
        assert!(
            temp_leftovers(dir.path()).is_empty(),
            "temps left behind: {:?}",
            temp_leftovers(dir.path())
        );
    }

    /// The partial-install regression: with the conflict on a *later* final
    /// path, the old loop had already replaced the first pre-existing `.rev`
    /// when the install failed. The transactional install must leave it
    /// byte-identical and remove every temp.
    #[test]
    fn streaming_build_partial_install_rolls_back_existing_revs() {
        let dir = tempfile::tempdir().unwrap();
        let volumes = write_fake_volumes(dir.path(), &[1024, 1024]);
        let existing = dir.path().join("set.part2_2_1.rev");
        let keep = b"pre-existing parity one".to_vec();
        std::fs::write(&existing, &keep).unwrap();
        // Occupy the second final path so the install cannot complete after
        // the first `.rev` would have been replaced.
        std::fs::create_dir(dir.path().join("set.part2_2_2.rev")).unwrap();

        let error = build_recovery_volumes_for_set_chunked(&volumes, 2, 64).unwrap_err();
        assert!(matches!(error, RarError::Io(_)), "got {error}");
        assert_eq!(
            std::fs::read(&existing).unwrap(),
            keep,
            "the first .rev must be rolled back"
        );
        assert!(
            dir.path().join("set.part2_2_2.rev").is_dir(),
            "the conflicting directory must stay untouched"
        );
        assert!(
            temp_leftovers(dir.path()).is_empty(),
            "temps left behind: {:?}",
            temp_leftovers(dir.path())
        );
        assert!(
            commit_leftovers(dir.path()).is_empty(),
            "commit leftovers: {:?}",
            commit_leftovers(dir.path())
        );
    }

    /// A failure inside the install transaction (the journal temp path is
    /// occupied) leaves every pre-existing `.rev` untouched and sweeps all
    /// staged temps.
    #[test]
    fn streaming_build_commit_failure_keeps_existing_revs_and_removes_temps() {
        let dir = tempfile::tempdir().unwrap();
        let volumes = write_fake_volumes(dir.path(), &[1024, 1024]);
        let first = dir.path().join("set.part2_2_1.rev");
        let second = dir.path().join("set.part2_2_2.rev");
        let keep_first = b"pre-existing parity one".to_vec();
        let keep_second = b"pre-existing parity two".to_vec();
        std::fs::write(&first, &keep_first).unwrap();
        std::fs::write(&second, &keep_second).unwrap();
        // `commit_files` writes its journal through this exact sibling name.
        std::fs::create_dir(dir.path().join(".set.rar5commit.journal.tmp")).unwrap();

        let error = build_recovery_volumes_for_set_chunked(&volumes, 2, 64).unwrap_err();
        assert!(matches!(error, RarError::Io(_)), "got {error}");
        assert_eq!(std::fs::read(&first).unwrap(), keep_first);
        assert_eq!(std::fs::read(&second).unwrap(), keep_second);
        // Drop the planted conflict so only transaction leftovers remain.
        std::fs::remove_dir(dir.path().join(".set.rar5commit.journal.tmp")).unwrap();
        assert!(
            temp_leftovers(dir.path()).is_empty(),
            "temps left behind: {:?}",
            temp_leftovers(dir.path())
        );
        assert!(
            commit_leftovers(dir.path()).is_empty(),
            "commit leftovers: {:?}",
            commit_leftovers(dir.path())
        );
    }

    /// A failure after the damaged original was parked as `*.bad` (the
    /// staged rebuild cannot be synced) must restore the original bytes and
    /// leave no `.bad` copy behind.
    #[test]
    fn failed_install_after_bad_rename_restores_the_damaged_original() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("set.part2.rar");
        let damaged = b"damaged volume bytes".to_vec();
        std::fs::write(&final_path, &damaged).unwrap();
        // The staged rebuild never exists, so the commit fails after
        // `final_path` was renamed to `set.part2.rar.bad`.
        let missing_tmp = dir.path().join(".set.part2.rar.rar5tmp-x");
        let handle = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&final_path)
            .unwrap();

        let error = commit_rebuilt_volumes(
            std::slice::from_ref(&final_path),
            &[0],
            vec![(0, missing_tmp, handle)],
            1,
            0,
        )
        .unwrap_err();
        assert!(matches!(error, RarError::Io(_)), "got {error}");
        assert_eq!(
            std::fs::read(&final_path).unwrap(),
            damaged,
            "the damaged original must be restored"
        );
        assert!(
            !dir.path().join("set.part2.rar.bad").exists(),
            "the parked copy must be moved back, not left behind"
        );
        assert!(
            temp_leftovers(dir.path()).is_empty(),
            "temps left behind: {:?}",
            temp_leftovers(dir.path())
        );
    }

    /// On success the damaged original stays parked as `*.bad` and the
    /// rebuilt temp lands at the final path.
    #[test]
    fn install_rebuilt_volume_keeps_the_damaged_original_as_bad() {
        let dir = tempfile::tempdir().unwrap();
        let final_path = dir.path().join("set.part2.rar");
        std::fs::write(&final_path, b"damaged").unwrap();
        let tmp = dir.path().join(".set.part2.rar.rar5tmp-x");
        std::fs::write(&tmp, b"rebuilt").unwrap();
        let handle = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&tmp)
            .unwrap();

        let rebuilt = commit_rebuilt_volumes(
            std::slice::from_ref(&final_path),
            &[0],
            vec![(0, tmp.clone(), handle)],
            1,
            0,
        )
        .unwrap();
        assert_eq!(rebuilt, vec![final_path.clone()]);
        assert_eq!(std::fs::read(&final_path).unwrap(), b"rebuilt");
        assert_eq!(
            std::fs::read(dir.path().join("set.part2.rar.bad")).unwrap(),
            b"damaged"
        );
        assert!(!tmp.exists(), "the staged temp must be consumed");
    }

    /// Trailer-format builds append a valid trailer after the streamed
    /// payload, and the rebuild path ignores those seven non-parity bytes.
    #[test]
    fn streaming_trailer_build_writes_valid_trailer() {
        let dir = tempfile::tempdir().unwrap();
        let volumes = write_fake_volumes(dir.path(), &[1024, 900, 1100]);
        // Zero tails select the trailer layout (WinRAR's ENDARC padding).
        for path in &volumes {
            let mut bytes = std::fs::read(path).unwrap();
            bytes.extend_from_slice(&[0u8; TRAILER_LEN]);
            std::fs::write(path, &bytes).unwrap();
        }
        let originals: Vec<Vec<u8>> = volumes
            .iter()
            .map(|path| std::fs::read(path).unwrap())
            .collect();
        let shard_len = originals.iter().map(Vec::len).max().unwrap();

        let written = build_recovery_volumes_for_set_chunked(&volumes, 2, 64).unwrap();
        assert_eq!(written.len(), 2);
        for (k, path) in written.iter().enumerate() {
            assert_eq!(
                path.file_name().unwrap().to_string_lossy(),
                format!("set.part{}.rev", k + 1),
                "the builder must return the final paths"
            );
            let bytes = std::fs::read(path).unwrap();
            assert_eq!(bytes.len(), shard_len);
            let mut file = std::fs::File::open(path).unwrap();
            assert_eq!(
                super::trailer::parse_trailer_reader(&mut file).unwrap(),
                Some(Meta {
                    data_count: 3,
                    rec_count: 2,
                    recovery_index: k,
                })
            );
        }
        assert!(
            temp_leftovers(dir.path()).is_empty(),
            "successful build left temps: {:?}",
            temp_leftovers(dir.path())
        );

        let missing = originals[2].clone();
        std::fs::remove_file(&volumes[2]).unwrap();
        let rebuilt = rebuild_missing_volumes_chunked(&volumes[0], None, None, 64).unwrap();
        assert_eq!(rebuilt, vec![volumes[2].clone()]);
        assert_eq!(std::fs::read(&volumes[2]).unwrap(), missing);
    }

    #[test]
    fn long_trailing_groups_do_not_overflow() {
        // A 20-digit group used to panic in `10usize.pow(20)` while
        // enumerating the ambiguous splits of a `.rev` name.
        let candidates = rev_name_candidates("mv4_10000000000000000000_1_1.rev");
        assert!(
            candidates
                .iter()
                .all(|candidate| !candidate.base.is_empty())
        );
    }

    /// `set44_2_1.rev` reads as `set` plus data volume 44 or `set4` plus
    /// volume 4; the scan must follow the data set that is actually on disk,
    /// so a stale `.rev` never claims another set's parity files.
    #[test]
    fn ambiguous_legacy_names_score_the_existing_data_set() {
        let dir = tempfile::tempdir().unwrap();
        let rev_name = "set44_2_1.rev";
        let base = |parent: &Path| identify(&parent.join(rev_name)).unwrap().1.base;

        // Only `set4`'s four data volumes exist.
        for index in 0..4 {
            std::fs::write(volume_path_rar4(dir.path(), "set4", index + 1), b"x").unwrap();
        }
        assert_eq!(base(dir.path()), "set4");
        assert!(rev_name_belongs_to_set(dir.path(), "set4", rev_name));
        assert!(!rev_name_belongs_to_set(dir.path(), "set", rev_name));

        // `set`'s own 44 volumes appear; the larger existing set wins now.
        for index in 0..44 {
            std::fs::write(volume_path_rar4(dir.path(), "set", index + 1), b"x").unwrap();
        }
        assert_eq!(base(dir.path()), "set");
        assert!(rev_name_belongs_to_set(dir.path(), "set", rev_name));
        assert!(!rev_name_belongs_to_set(dir.path(), "set4", rev_name));

        // Matching is ASCII case-insensitive, like the official tools on
        // Windows, and a foreign base never matches.
        assert!(rev_name_belongs_to_set(dir.path(), "set", "SET44_2_1.REV"));
        assert!(!rev_name_belongs_to_set(
            dir.path(),
            "set4",
            "other4_2_1.rev"
        ));
    }
}

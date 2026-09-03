/// RAR5 canonical Huffman codec.
///
/// Two-level decode: quick table for codes up to QUICK_BITS, slower scan
/// for longer codes. Based on the structure used in libarchive's RAR5 reader.
use super::bitstream::{BitReader, BitWriter};
use super::lzss_huff::{MAX_CODE_LENGTH, QUICK_BITS, QUICK_SIZE};

/// Huffman tree node: (frequency, symbol/node id, children).
type HuffNode = (u32, usize, Option<(usize, usize)>);

// ── Decode Table ───────────────────────────────────────────────────────────

pub struct DecodeTable {
    pub num_symbols: usize,
    decode_len: [u32; MAX_CODE_LENGTH + 2],
    decode_pos: [usize; MAX_CODE_LENGTH + 2],
    decode_num: Vec<u16>,
    quick_len: Vec<u8>,
    quick_num: Vec<u16>,
}

impl DecodeTable {
    pub fn new(code_lengths: &[u8]) -> Self {
        let n = code_lengths.len();

        // Count codes of each length
        let mut len_count = [0u32; MAX_CODE_LENGTH + 2];
        for &cl in code_lengths {
            let cl = cl as usize;
            if cl > 0 && cl <= MAX_CODE_LENGTH {
                len_count[cl] += 1;
            }
        }

        let mut decode_len = [0u32; MAX_CODE_LENGTH + 2];
        let mut decode_pos = [0usize; MAX_CODE_LENGTH + 2];
        let mut decode_num = vec![0u16; n.max(1)];

        let mut code: u32 = 0;
        let mut pos: usize = 0;
        for i in 1..=MAX_CODE_LENGTH {
            code <<= 1;
            decode_len[i - 1] = code << (MAX_CODE_LENGTH - i);
            decode_pos[i] = pos;
            code += len_count[i];
            pos += len_count[i] as usize;
        }
        decode_len[MAX_CODE_LENGTH] = 1 << MAX_CODE_LENGTH;

        // Fill decode_num
        let mut pos_tracker = decode_pos;
        for (sym, item) in code_lengths.iter().enumerate().take(n) {
            let cl = *item as usize;
            if cl > 0 && cl <= MAX_CODE_LENGTH {
                decode_num[pos_tracker[cl]] = sym as u16;
                pos_tracker[cl] += 1;
            }
        }

        // Build quick lookup table
        let mut quick_len = vec![0u8; QUICK_SIZE];
        let mut quick_num = vec![0u16; QUICK_SIZE];

        let mut code: u32 = 0;
        for cl in 1..=QUICK_BITS {
            let start_pos = decode_pos[cl];
            for j in 0..len_count[cl] as usize {
                let sym = decode_num[start_pos + j];
                let prefix = code << (QUICK_BITS - cl);
                let fill = 1 << (QUICK_BITS - cl);
                for k in 0..fill {
                    let entry = (prefix + k) as usize;
                    if entry < QUICK_SIZE {
                        quick_len[entry] = cl as u8;
                        quick_num[entry] = sym;
                    }
                }
                code += 1;
            }
            code <<= 1;
        }

        DecodeTable {
            num_symbols: n,
            decode_len,
            decode_pos,
            decode_num,
            quick_len,
            quick_num,
        }
    }
}

/// Decode one Huffman symbol from the bitstream.
pub fn decode_symbol(table: &DecodeTable, reader: &mut BitReader) -> Result<usize, &'static str> {
    let bits_avail = reader.bits_remaining();
    if bits_avail == 0 {
        return Err("Huffman decode: no bits remaining");
    }

    // Try quick lookup
    let peek_count = (QUICK_BITS).min(bits_avail) as u8;
    let mut prefix = reader.peek_bits(peek_count)?;
    if (peek_count as usize) < QUICK_BITS {
        prefix <<= QUICK_BITS as u32 - peek_count as u32;
    }

    let cl = table.quick_len[prefix as usize];
    if cl > 0 && cl <= peek_count {
        reader.skip_bits(cl as u32);
        return Ok(table.quick_num[prefix as usize] as usize);
    }

    // Slow path
    let peek_count = (MAX_CODE_LENGTH).min(bits_avail) as u8;
    let mut bits = reader.peek_bits(peek_count)?;
    if (peek_count as usize) < MAX_CODE_LENGTH {
        bits <<= MAX_CODE_LENGTH as u32 - peek_count as u32;
    }

    for i in 1..=MAX_CODE_LENGTH {
        if bits < table.decode_len[i] {
            reader.skip_bits(i as u32);
            let prev_boundary = if i > 1 { table.decode_len[i - 1] } else { 0 };
            let offset = ((bits - prev_boundary) >> (MAX_CODE_LENGTH - i)) as usize;
            let idx = table.decode_pos[i] + offset;
            if idx < table.num_symbols {
                return Ok(table.decode_num[idx] as usize);
            }
            break;
        }
    }

    Err("Huffman decode: invalid code")
}

// ── Encode Table ───────────────────────────────────────────────────────────

pub struct EncodeTable {
    pub codes: Vec<u32>,
    pub lengths: Vec<u8>,
}

impl EncodeTable {
    pub fn new(code_lengths: &[u8]) -> Self {
        let n = code_lengths.len();

        let mut len_count = [0u32; MAX_CODE_LENGTH + 2];
        for &cl in code_lengths {
            let cl = cl as usize;
            if cl > 0 && cl <= MAX_CODE_LENGTH {
                len_count[cl] += 1;
            }
        }

        let mut code: u32 = 0;
        let mut next_code = [0u32; MAX_CODE_LENGTH + 2];
        for bits in 1..=MAX_CODE_LENGTH {
            code <<= 1;
            next_code[bits] = code;
            code += len_count[bits];
        }

        let mut codes = vec![0u32; n];
        for sym in 0..n {
            let cl = code_lengths[sym] as usize;
            if cl > 0 && cl <= MAX_CODE_LENGTH {
                codes[sym] = next_code[cl];
                next_code[cl] += 1;
            }
        }

        EncodeTable {
            codes,
            lengths: code_lengths.to_vec(),
        }
    }
}

/// Encode one Huffman symbol to the bitstream.
pub fn encode_symbol(table: &EncodeTable, writer: &mut BitWriter, symbol: usize) {
    let cl = table.lengths[symbol];
    debug_assert!(cl > 0, "cannot encode symbol {symbol}: zero length");
    writer.write_bits(table.codes[symbol], cl);
}

/// Build optimal Huffman code lengths from symbol frequencies.
/// Returns a Vec of code bit-lengths (0 for unused symbols).
pub fn build_code_lengths_from_freqs(freqs: &[u32], max_length: usize) -> Vec<u8> {
    let n = freqs.len();
    let active: Vec<(u32, usize)> = freqs
        .iter()
        .enumerate()
        .filter(|(_, f)| **f > 0)
        .map(|(i, &f)| (f, i))
        .collect();

    if active.is_empty() {
        return vec![0; n];
    }
    if active.len() == 1 {
        let mut lengths = vec![0u8; n];
        lengths[active[0].1] = 1;
        return lengths;
    }

    // Build Huffman tree using sorted merge (no BinaryHeap needed for Node)
    // Two-queue approach: one for leaves, one for internal nodes
    let mut leaves: Vec<HuffNode> = active
        .iter()
        .map(|&(freq, sym)| (freq, sym, None))
        .collect();
    leaves.sort_by_key(|&(f, s, _)| (f, s));

    // nodes stores: (freq, node_id, children)
    let mut nodes: Vec<HuffNode> = Vec::new();
    let mut all_nodes: Vec<HuffNode> = Vec::new();
    // Copy leaves into all_nodes
    for &(f, s, _) in &leaves {
        all_nodes.push((f, s, None));
    }

    let mut li = 0; // leaf index
    let mut ni = 0; // internal node index
    let mut counter = n;

    fn pick_min(
        leaves: &[HuffNode],
        li: &mut usize,
        nodes: &[HuffNode],
        ni: &mut usize,
    ) -> (u32, usize) {
        let have_leaf = *li < leaves.len();
        let have_node = *ni < nodes.len();
        if have_leaf && have_node {
            if leaves[*li].0 <= nodes[*ni].0 {
                let idx = *li;
                *li += 1;
                (leaves[idx].0, idx)
            } else {
                let idx = *ni;
                *ni += 1;
                (nodes[idx].0, leaves.len() + idx)
            }
        } else if have_leaf {
            let idx = *li;
            *li += 1;
            (leaves[idx].0, idx)
        } else {
            let idx = *ni;
            *ni += 1;
            (nodes[idx].0, leaves.len() + idx)
        }
    }

    let total_leaves = leaves.len();
    while (total_leaves - li) + (nodes.len() - ni) > 1 {
        let (f1, id1) = pick_min(&leaves, &mut li, &nodes, &mut ni);
        let (f2, id2) = pick_min(&leaves, &mut li, &nodes, &mut ni);
        counter += 1;
        nodes.push((f1 + f2, counter, Some((id1, id2))));
    }

    // Rebuild all_nodes with internal nodes appended
    for &(f, id, children) in &nodes {
        all_nodes.push((f, id, children));
    }

    let mut lengths = vec![0u8; n];

    // Walk the tree to assign depths
    fn walk(all_nodes: &[HuffNode], node_idx: usize, depth: u8, lengths: &mut Vec<u8>) {
        if let Some((left, right)) = all_nodes[node_idx].2 {
            walk(all_nodes, left, depth + 1, lengths);
            walk(all_nodes, right, depth + 1, lengths);
        } else {
            // Leaf: all_nodes[node_idx].1 is the original symbol
            let sym = all_nodes[node_idx].1;
            if sym < lengths.len() {
                lengths[sym] = depth;
            }
        }
    }

    let root_idx = all_nodes.len() - 1;
    walk(&all_nodes, root_idx, 0, &mut lengths);

    // Enforce max_length and fix Kraft inequality.
    // Clamping depths > max_length to max_length makes the code overcomplete
    // (Kraft sum > 1). To fix, we lengthen some shorter codes (increase their
    // bit length) which reduces their Kraft contribution.
    let max_len = max_length as u8;
    let mut needs_fix = false;
    for l in &mut lengths {
        if *l > max_len {
            *l = max_len;
            needs_fix = true;
        }
    }

    if needs_fix {
        // Clamping depths > max_length to max_length makes the code
        // overcomplete (Kraft sum > 1). Remove the excess exactly: each step
        // moves one code from the longest used length below the limit to
        // length+1 and shortens one overflow code (currently at max_length)
        // to that same length, which reduces the Kraft sum by exactly 1.
        let max_len = max_length as u8;
        let target: i64 = 1i64 << max_len;
        let kraft = |lengths: &[u8]| -> i64 {
            lengths
                .iter()
                .filter(|&&l| l > 0)
                .map(|&l| 1i64 << (max_len - l))
                .sum()
        };
        let mut excess = kraft(&lengths) - target;
        let mut syms_by_len: Vec<Vec<usize>> = vec![Vec::new(); max_len as usize + 1];
        for (i, &l) in lengths.iter().enumerate() {
            if l > 0 {
                syms_by_len[l as usize].push(i);
            }
        }
        while excess > 0 {
            // Longest used length strictly below the limit.
            let mut bits = max_len - 1;
            while bits > 0 && syms_by_len[bits as usize].is_empty() {
                bits -= 1;
            }
            if bits == 0 || syms_by_len[max_len as usize].is_empty() {
                break; // cannot resolve exactly; leave as-is
            }
            // Move one code: bits -> bits + 1.
            let sym = syms_by_len[bits as usize].pop().unwrap();
            lengths[sym] += 1;
            syms_by_len[(bits + 1) as usize].push(sym);
            // Shorten one overflow code to bits + 1.
            let sym2 = syms_by_len[max_len as usize].pop().unwrap();
            lengths[sym2] = bits + 1;
            syms_by_len[(bits + 1) as usize].push(sym2);
            excess -= 1;
        }
    }

    lengths
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::lzss_huff::HUFF_NC;

    // Regression test for the length-limited Huffman correction.
    //
    // This skewed 306-symbol frequency distribution was captured from a real
    // binary archive (a bundled 7-Zip binary). It forces Huffman depths
    // beyond MAX_CODE_LENGTH. The old clamp + greedy "lengthen the shortest
    // code" fix could stop with an INCOMPLETE code (Kraft sum < 2^15), which
    // 7-Zip's RAR5 decoder rejects ("Data Error") while rar-rs's own decoder
    // and The Unarchiver tolerate. The exact correction must always land on
    // a complete code (Kraft sum == 2^MAX_CODE_LENGTH).
    const FREQS: &[(usize, u32)] = &[
        (0, 775),
        (1, 236),
        (2, 125),
        (3, 136),
        (4, 177),
        (5, 233),
        (6, 107),
        (7, 94),
        (8, 191),
        (9, 120),
        (10, 110),
        (11, 103),
        (12, 119),
        (13, 81),
        (14, 79),
        (15, 381),
        (16, 237),
        (17, 77),
        (18, 87),
        (19, 68),
        (20, 114),
        (21, 83),
        (22, 69),
        (23, 80),
        (24, 164),
        (25, 82),
        (26, 67),
        (27, 79),
        (28, 90),
        (29, 79),
        (30, 242),
        (31, 316),
        (32, 262),
        (33, 75),
        (34, 52),
        (35, 48),
        (36, 215),
        (37, 73),
        (38, 60),
        (39, 60),
        (40, 107),
        (41, 78),
        (42, 49),
        (43, 71),
        (44, 71),
        (45, 62),
        (46, 45),
        (47, 49),
        (48, 120),
        (49, 142),
        (50, 48),
        (51, 121),
        (52, 62),
        (53, 60),
        (54, 53),
        (55, 75),
        (56, 128),
        (57, 110),
        (58, 47),
        (59, 69),
        (60, 68),
        (61, 84),
        (62, 46),
        (63, 49),
        (64, 120),
        (65, 257),
        (66, 65),
        (67, 81),
        (68, 194),
        (69, 130),
        (70, 78),
        (71, 92),
        (72, 511),
        (73, 167),
        (74, 62),
        (75, 50),
        (76, 167),
        (77, 81),
        (78, 46),
        (79, 75),
        (80, 108),
        (81, 55),
        (82, 47),
        (83, 66),
        (84, 66),
        (85, 64),
        (86, 50),
        (87, 71),
        (88, 82),
        (89, 51),
        (90, 48),
        (91, 50),
        (92, 73),
        (93, 71),
        (94, 51),
        (95, 50),
        (96, 99),
        (97, 62),
        (98, 55),
        (99, 58),
        (100, 65),
        (101, 73),
        (102, 100),
        (103, 54),
        (104, 77),
        (105, 50),
        (106, 48),
        (107, 45),
        (108, 58),
        (109, 45),
        (110, 42),
        (111, 78),
        (112, 95),
        (113, 53),
        (114, 69),
        (115, 63),
        (116, 176),
        (117, 207),
        (118, 65),
        (119, 83),
        (120, 103),
        (121, 53),
        (122, 46),
        (123, 82),
        (124, 84),
        (125, 94),
        (126, 84),
        (127, 102),
        (128, 90),
        (129, 93),
        (130, 52),
        (131, 235),
        (132, 193),
        (133, 224),
        (134, 63),
        (135, 71),
        (136, 101),
        (137, 544),
        (138, 53),
        (139, 353),
        (140, 57),
        (141, 260),
        (142, 61),
        (143, 60),
        (144, 124),
        (145, 75),
        (146, 54),
        (147, 59),
        (148, 64),
        (149, 57),
        (150, 61),
        (151, 67),
        (152, 74),
        (153, 52),
        (154, 64),
        (155, 52),
        (156, 58),
        (157, 65),
        (158, 67),
        (159, 64),
        (160, 89),
        (161, 74),
        (162, 69),
        (163, 62),
        (164, 64),
        (165, 64),
        (166, 47),
        (167, 56),
        (168, 71),
        (169, 50),
        (170, 67),
        (171, 57),
        (172, 85),
        (173, 53),
        (174, 63),
        (175, 51),
        (176, 84),
        (177, 57),
        (178, 63),
        (179, 48),
        (180, 72),
        (181, 74),
        (182, 125),
        (183, 72),
        (184, 93),
        (185, 65),
        (186, 64),
        (187, 73),
        (188, 89),
        (189, 135),
        (190, 120),
        (191, 70),
        (192, 170),
        (193, 197),
        (194, 112),
        (195, 118),
        (196, 113),
        (197, 142),
        (198, 117),
        (199, 201),
        (200, 105),
        (201, 84),
        (202, 82),
        (203, 60),
        (204, 66),
        (205, 73),
        (206, 80),
        (207, 56),
        (208, 131),
        (209, 87),
        (210, 105),
        (211, 87),
        (212, 71),
        (213, 74),
        (214, 78),
        (215, 71),
        (216, 102),
        (217, 67),
        (218, 69),
        (219, 83),
        (220, 103),
        (221, 103),
        (222, 80),
        (223, 129),
        (224, 133),
        (225, 87),
        (226, 93),
        (227, 72),
        (228, 80),
        (229, 79),
        (230, 76),
        (231, 111),
        (232, 1608),
        (233, 312),
        (234, 79),
        (235, 282),
        (236, 71),
        (237, 93),
        (238, 97),
        (239, 189),
        (240, 113),
        (241, 68),
        (242, 64),
        (243, 81),
        (244, 67),
        (245, 61),
        (246, 96),
        (247, 129),
        (248, 117),
        (249, 95),
        (250, 85),
        (251, 95),
        (252, 101),
        (253, 111),
        (254, 128),
        (255, 238),
        (258, 2424),
        (259, 455),
        (260, 79),
        (261, 55),
        (262, 758),
        (263, 1627),
        (264, 2190),
        (265, 1364),
        (266, 1081),
        (267, 942),
        (268, 698),
        (269, 569),
        (270, 703),
        (271, 332),
        (272, 176),
        (273, 160),
        (274, 161),
        (275, 27),
        (276, 9),
        (277, 4),
        (278, 4),
        (279, 2),
        (280, 3),
        (281, 2),
        (282, 1),
        (283, 2),
        (285, 1),
        (289, 1),
        (303, 1),
    ];

    #[test]
    fn length_limited_huffman_stays_complete() {
        let mut freqs = vec![0u32; HUFF_NC];
        for &(sym, f) in FREQS {
            freqs[sym] = f;
        }
        let lengths = build_code_lengths_from_freqs(&freqs, MAX_CODE_LENGTH);
        let kraft: i64 = lengths
            .iter()
            .filter(|&&l| l > 0)
            .map(|&l| 1i64 << (MAX_CODE_LENGTH - l as usize))
            .sum();
        assert!(lengths.iter().all(|&l| l <= MAX_CODE_LENGTH as u8));
        assert_eq!(kraft, 1i64 << MAX_CODE_LENGTH);
    }
}

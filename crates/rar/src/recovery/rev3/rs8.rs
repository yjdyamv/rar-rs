//! GF(2^8) Reed-Solomon codec for the legacy `.rev` recovery volumes
//! (RAR 1.5–4.x), ported from `rars`' `recovery/rar3.rs` (MIT OR
//! Apache-2.0 per the rars workspace metadata; the upstream file carries no
//! header — see NOTICE).
//!
//! A codeword holds one symbol per volume (data volumes first, then parity
//! volumes); the codec is the shortened systematic RS(255) code over
//! GF(2^8) with primitive polynomial `0x11d` that WinRAR's RAR3 recovery
//! machinery uses. Encoding a data column yields the `parity_size` parity
//! symbols stored in the corresponding `.rev` volumes.

const MAX_PARITY: usize = 255;
const MAX_POLYNOMIAL: usize = 512;
const PRIMITIVE_POLYNOMIAL: u16 = 0x11d;

/// The largest codeword (data + recovery volumes) the codec can hold.
pub(crate) const MAX_CODEWORD: usize = MAX_PARITY;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Rs8Error {
    InvalidParitySize,
    InvalidCodewordSize,
    TooManyErasures,
    DecodeFailed,
    Uncorrectable,
}

impl std::fmt::Display for Rs8Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidParitySize => f.write_str("legacy recovery parity size is invalid"),
            Self::InvalidCodewordSize => f.write_str("legacy recovery codeword size is invalid"),
            Self::TooManyErasures => {
                f.write_str("legacy recovery data cannot repair this many erasures")
            }
            Self::DecodeFailed => f.write_str("legacy recovery decode failed"),
            Self::Uncorrectable => {
                f.write_str("legacy recovery damage cannot be located from the parity")
            }
        }
    }
}

pub(crate) type Rs8Result<T> = std::result::Result<T, Rs8Error>;

pub(crate) struct Rsc8 {
    parity_size: usize,
    gf_exp: [u8; MAX_POLYNOMIAL],
    gf_log: [u16; MAX_PARITY + 1],
    generator: Vec<u8>,
}

impl Rsc8 {
    pub(crate) fn new(parity_size: usize) -> Rs8Result<Self> {
        if parity_size == 0 || parity_size > MAX_PARITY {
            return Err(Rs8Error::InvalidParitySize);
        }
        let mut coder = Self {
            parity_size,
            gf_exp: [0; MAX_POLYNOMIAL],
            gf_log: [0; MAX_PARITY + 1],
            generator: vec![0; parity_size],
        };
        coder.init_field();
        coder.init_generator();
        Ok(coder)
    }

    /// Encode one data column (one symbol per data volume) into
    /// `parity_size` parity symbols (one per recovery volume).
    pub(crate) fn encode(&self, data: &[u8]) -> Vec<u8> {
        let mut shift = vec![0u8; self.parity_size + 1];
        for &byte in data {
            let feedback = byte ^ shift[self.parity_size - 1];
            for index in (1..self.parity_size).rev() {
                shift[index] = shift[index - 1] ^ self.mul(self.generator[index], feedback);
            }
            shift[0] = self.mul(self.generator[0], feedback);
        }
        (0..self.parity_size)
            .map(|index| shift[self.parity_size - index - 1])
            .collect()
    }

    /// The `parity_size` syndrome symbols of a codeword; all zero means the
    /// word satisfies the code's parity equations.
    pub(crate) fn syndromes(&self, codeword: &[u8]) -> Vec<u8> {
        (0..self.parity_size)
            .map(|index| {
                let factor = self.gf_exp[index + 1];
                let mut sum = 0u8;
                for &byte in codeword {
                    sum = byte ^ self.mul(factor, sum);
                }
                sum
            })
            .collect()
    }

    /// Fill the symbols at the (known) `erasures` positions from the
    /// surviving symbols. An all-zero syndrome word is already a codeword
    /// and is returned unchanged.
    pub(crate) fn correct_erasures(
        &self,
        codeword: &mut [u8],
        erasures: &[usize],
    ) -> Rs8Result<()> {
        if codeword.is_empty() || codeword.len() > MAX_PARITY {
            return Err(Rs8Error::InvalidCodewordSize);
        }
        if erasures.len() > self.parity_size {
            return Err(Rs8Error::TooManyErasures);
        }
        if erasures.iter().any(|&index| index >= codeword.len()) {
            return Err(Rs8Error::InvalidCodewordSize);
        }

        let syndromes = self.syndromes(codeword);
        if syndromes.iter().all(|&value| value == 0) {
            return Ok(());
        }
        if erasures.is_empty() {
            return Err(Rs8Error::DecodeFailed);
        }

        let mut locator = vec![0u8; self.parity_size + 1];
        locator[0] = 1;
        for &erasure in erasures {
            let multiplier = self.gf_exp[codeword.len() - erasure - 1];
            for index in (1..=self.parity_size).rev() {
                locator[index] ^= self.mul(multiplier, locator[index - 1]);
            }
        }

        let mut error_locs = Vec::new();
        let mut denominators = Vec::new();
        for root in (MAX_PARITY - codeword.len())..=MAX_PARITY {
            let mut sum = 0;
            for (power, &coefficient) in locator.iter().enumerate() {
                sum ^= self.mul(self.gf_exp[(power * root) % MAX_PARITY], coefficient);
            }
            if sum == 0 {
                let loc = MAX_PARITY - root;
                error_locs.push(loc);
                let mut denominator = 0;
                for index in (1..=self.parity_size).step_by(2) {
                    denominator ^= self.mul(
                        locator[index],
                        self.gf_exp[(root * (index - 1)) % MAX_PARITY],
                    );
                }
                denominators.push(denominator);
            }
        }
        if error_locs.is_empty() || error_locs.len() > self.parity_size {
            return Err(Rs8Error::DecodeFailed);
        }

        let evaluator = self.multiply_polynomials(&locator, &syndromes);
        for (&loc, &denominator) in error_locs.iter().zip(&denominators) {
            if denominator == 0 {
                return Err(Rs8Error::DecodeFailed);
            }
            let data_pos = codeword
                .len()
                .checked_sub(loc + 1)
                .ok_or(Rs8Error::DecodeFailed)?;
            let dloc = MAX_PARITY - loc;
            let mut numerator = 0;
            for (index, &coefficient) in evaluator.iter().enumerate() {
                numerator ^= self.mul(coefficient, self.gf_exp[(dloc * index) % MAX_PARITY]);
            }
            let correction = self.mul(
                numerator,
                self.gf_exp[MAX_PARITY - usize::from(self.gf_log[denominator as usize])],
            );
            codeword[data_pos] ^= correction;
        }
        Ok(())
    }

    /// Locate unknown errors from a nonzero syndrome word (Berlekamp-Massey
    /// plus a Chien search), returning their codeword positions. At most
    /// `floor(parity_size / 2)` errors can be located; the positions can
    /// then be corrected as erasures with [`Self::correct_erasures`].
    pub(crate) fn locate_errors(
        &self,
        syndromes: &[u8],
        codeword_len: usize,
    ) -> Rs8Result<Vec<usize>> {
        if syndromes.len() > self.parity_size || codeword_len == 0 || codeword_len > MAX_PARITY {
            return Err(Rs8Error::InvalidCodewordSize);
        }
        if syndromes.len() < 2 {
            return Err(Rs8Error::Uncorrectable);
        }

        // The syndrome sequence is `s_k = sum_i Y_i * X_i^k`; the error
        // locator polynomial has roots at `X_i^-1`.
        let mut locator = vec![1u8];
        let mut previous = vec![1u8];
        let mut degree = 0usize;
        let mut shift = 1usize;
        let mut previous_discrepancy = 1u8;
        for n in 0..syndromes.len() {
            let mut discrepancy = syndromes[n];
            for index in 1..=degree {
                discrepancy ^= self.mul(locator[index], syndromes[n - index]);
            }
            if discrepancy == 0 {
                shift += 1;
                continue;
            }
            let saved = locator.clone();
            let scale = self.mul(
                discrepancy,
                self.gf_exp[MAX_PARITY - usize::from(self.gf_log[previous_discrepancy as usize])],
            );
            if locator.len() < previous.len() + shift {
                locator.resize(previous.len() + shift, 0);
            }
            for index in 0..previous.len() {
                locator[index + shift] ^= self.mul(scale, previous[index]);
            }
            if 2 * degree <= n {
                degree = n + 1 - degree;
                previous = saved;
                previous_discrepancy = discrepancy;
                shift = 1;
            } else {
                shift += 1;
            }
        }
        if degree == 0 || 2 * degree > syndromes.len() {
            return Err(Rs8Error::Uncorrectable);
        }
        locator.truncate(degree + 1);

        // Chien search over the codeword positions: position `i` corresponds
        // to `X_i = alpha^(codeword_len - 1 - i)` and the locator has roots
        // at `X_i^-1`.
        let mut positions = Vec::new();
        for index in 0..codeword_len {
            let x = self.gf_exp[(MAX_PARITY + index + 1 - codeword_len) % MAX_PARITY];
            let mut sum = 0u8;
            for &coefficient in locator.iter().rev() {
                sum = self.mul(sum, x) ^ coefficient;
            }
            if sum == 0 {
                positions.push(index);
            }
        }
        if positions.len() != degree {
            return Err(Rs8Error::Uncorrectable);
        }
        Ok(positions)
    }

    fn init_field(&mut self) {
        let mut value = 1u16;
        for index in 0..MAX_PARITY {
            self.gf_log[value as usize] = index as u16;
            self.gf_exp[index] = value as u8;
            value <<= 1;
            if value > 0xff {
                value ^= PRIMITIVE_POLYNOMIAL;
            }
        }
        for index in MAX_PARITY..MAX_POLYNOMIAL {
            self.gf_exp[index] = self.gf_exp[index - MAX_PARITY];
        }
    }

    fn init_generator(&mut self) {
        let mut current = vec![0u8; self.parity_size];
        current[0] = 1;
        for index in 1..=self.parity_size {
            let mut factor = vec![0u8; self.parity_size];
            factor[0] = self.gf_exp[index];
            if self.parity_size > 1 {
                factor[1] = 1;
            }
            self.generator = self.multiply_polynomials(&factor, &current);
            current.clone_from(&self.generator);
        }
    }

    fn multiply_polynomials(&self, left: &[u8], right: &[u8]) -> Vec<u8> {
        let mut out = vec![0u8; self.parity_size];
        for left_index in 0..self.parity_size {
            if left.get(left_index).copied().unwrap_or(0) == 0 {
                continue;
            }
            for right_index in 0..(self.parity_size - left_index) {
                out[left_index + right_index] ^= self.mul(
                    left[left_index],
                    right.get(right_index).copied().unwrap_or(0),
                );
            }
        }
        out
    }

    fn mul(&self, left: u8, right: u8) -> u8 {
        if left == 0 || right == 0 {
            0
        } else {
            self.gf_exp[usize::from(self.gf_log[left as usize] + self.gf_log[right as usize])]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Rs8Error, Rsc8};

    #[test]
    fn generator_matches_the_unrar_shape() {
        let coder = Rsc8::new(11).unwrap();
        assert_eq!(
            coder.generator,
            vec![97, 180, 203, 151, 195, 196, 219, 7, 113, 50, 69]
        );
    }

    #[test]
    fn erasure_correction_restores_data_and_parity_symbols() {
        let coder = Rsc8::new(5).unwrap();
        let data = b"rar3-rs8";
        let parity = coder.encode(data);
        let original = [data.as_slice(), parity.as_slice()].concat();

        let mut codeword = original.clone();
        codeword[1] = 0;
        codeword[7] = 0;
        codeword[10] = 0;
        coder.correct_erasures(&mut codeword, &[1, 7, 10]).unwrap();
        assert_eq!(codeword, original);
    }

    #[test]
    fn unknown_errors_are_located_and_corrected() {
        for parity_size in [2usize, 4, 8] {
            let coder = Rsc8::new(parity_size).unwrap();
            let data: Vec<u8> = (0..16u8).map(|value| value.wrapping_mul(31)).collect();
            let parity = coder.encode(&data);
            let original = [data.as_slice(), parity.as_slice()].concat();

            let positions_to_flip: Vec<usize> =
                (0..parity_size / 2).map(|index| index * 3 + 2).collect();
            let mut codeword = original.clone();
            for &position in &positions_to_flip {
                codeword[position] ^= 0x5a;
            }
            let syndromes = coder.syndromes(&codeword);
            let mut located = coder
                .locate_errors(&syndromes, codeword.len())
                .expect("errors locatable");
            located.sort_unstable();
            assert_eq!(located, positions_to_flip, "parity {parity_size}");
            coder.correct_erasures(&mut codeword, &located).unwrap();
            assert_eq!(codeword, original, "parity {parity_size}");
        }
    }

    #[test]
    fn every_single_error_position_is_locatable() {
        for codeword_len in [3usize, 5, 6, 18, 20] {
            for parity_size in [2usize, 4] {
                if codeword_len <= parity_size {
                    continue;
                }
                let coder = Rsc8::new(parity_size).unwrap();
                let data: Vec<u8> = (0..(codeword_len - parity_size) as u8)
                    .map(|value| value.wrapping_mul(37).wrapping_add(5))
                    .collect();
                let parity = coder.encode(&data);
                let original = [data.as_slice(), parity.as_slice()].concat();
                for flip in 0..codeword_len {
                    let mut codeword = original.clone();
                    codeword[flip] ^= 0x5a;
                    let syndromes = coder.syndromes(&codeword);
                    let located = coder
                        .locate_errors(&syndromes, codeword.len())
                        .unwrap_or_else(|error| {
                            panic!("len {codeword_len} parity {parity_size} flip {flip}: {error}")
                        });
                    assert_eq!(
                        located,
                        vec![flip],
                        "len {codeword_len} parity {parity_size} flip {flip}"
                    );
                    coder.correct_erasures(&mut codeword, &located).unwrap();
                    assert_eq!(codeword, original);
                }
            }
        }
    }

    #[test]
    fn too_many_erasures_are_rejected() {
        let coder = Rsc8::new(2).unwrap();
        let mut codeword = b"abcde".to_vec();
        assert_eq!(
            coder.correct_erasures(&mut codeword, &[0, 1, 2]),
            Err(Rs8Error::TooManyErasures)
        );
    }

    #[test]
    fn a_clean_codeword_is_left_unchanged() {
        let coder = Rsc8::new(3).unwrap();
        let data = b"clean";
        let parity = coder.encode(data);
        let mut codeword = [data.as_slice(), parity.as_slice()].concat();
        let original = codeword.clone();
        coder.correct_erasures(&mut codeword, &[]).unwrap();
        assert_eq!(codeword, original);
    }
}

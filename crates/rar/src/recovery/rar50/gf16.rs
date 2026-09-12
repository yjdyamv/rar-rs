//! GF(2^16) field, Cauchy encoder matrix and the parity encoder.
//!
//! Ported from the `rars` project (MIT OR Apache-2.0, upstream `c08a17b`);
//! the linear-system solver here is shared with the repair side.

use super::shared_gf16;
use super::{Error, FIELD_MASK, FIELD_SIZE, PRIMITIVE_POLYNOMIAL, Result, ZERO_LOG_SENTINEL};
pub(super) fn invert_linear_system_matrix(gf: &Gf16, matrix: &[Vec<u16>]) -> Result<Vec<Vec<u16>>> {
    let n = matrix.len();
    if matrix.len() != n || matrix.iter().any(|row| row.len() != n) {
        return Err(Error::BadRecoveryChunk);
    }
    let mut matrix = matrix.to_vec();
    let mut inverse = vec![vec![0u16; n]; n];
    for (row, inverse_row) in inverse.iter_mut().enumerate() {
        inverse_row[row] = 1;
    }

    for col in 0..n {
        let pivot = (col..n)
            .find(|&row| matrix[row][col] != 0)
            .ok_or(Error::SingularElement)?;
        matrix.swap(col, pivot);
        inverse.swap(col, pivot);
        let inv = gf.inv(matrix[col][col])?;
        for value in &mut matrix[col] {
            *value = gf.mul(*value, inv);
        }
        for value in &mut inverse[col] {
            *value = gf.mul(*value, inv);
        }

        let pivot_matrix_row = matrix[col].clone();
        let pivot_inverse_row = inverse[col].clone();
        for row in 0..n {
            if row == col {
                continue;
            }
            let factor = matrix[row][col];
            if factor == 0 {
                continue;
            }
            for (value, pivot) in matrix[row]
                .iter_mut()
                .zip(pivot_matrix_row.iter().copied())
                .skip(col)
            {
                *value ^= gf.mul(factor, pivot);
            }
            for (value, pivot) in inverse[row]
                .iter_mut()
                .zip(pivot_inverse_row.iter().copied())
            {
                *value ^= gf.mul(factor, pivot);
            }
        }
    }
    Ok(inverse)
}

pub(super) fn apply_inverse_matrix(
    gf: &Gf16,
    inverse: &[Vec<u16>],
    rhs: &[u16],
) -> Result<Vec<u16>> {
    if inverse.len() != rhs.len() || inverse.iter().any(|row| row.len() != rhs.len()) {
        return Err(Error::BadRecoveryChunk);
    }
    Ok(inverse
        .iter()
        .map(|row| {
            row.iter()
                .zip(rhs)
                .fold(0u16, |sum, (&coefficient, &value)| {
                    sum ^ gf.mul(coefficient, value)
                })
        })
        .collect())
}

pub(super) fn read_u32(input: &[u8], offset: usize) -> Result<u32> {
    input
        .get(offset..offset + 4)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or(Error::BadRecoveryChunk)
}

pub(super) fn read_u64(input: &[u8], offset: usize) -> Result<u64> {
    input
        .get(offset..offset + 8)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or(Error::BadRecoveryChunk)
}

#[derive(Debug, Clone)]

pub struct Gf16 {
    exp: Box<[u16]>,
    log: Box<[u32]>,
}

impl Gf16 {
    pub fn new() -> Self {
        let mut exp = vec![0u16; FIELD_SIZE * 4 + 1];
        let mut log = vec![0u32; FIELD_SIZE + 1];
        let mut value = 1u32;
        for power in 0..FIELD_SIZE {
            log[value as usize] = power as u32;
            exp[power] = value as u16;
            exp[power + FIELD_SIZE] = value as u16;
            value <<= 1;
            if value > FIELD_MASK {
                value ^= PRIMITIVE_POLYNOMIAL;
            }
        }
        log[0] = ZERO_LOG_SENTINEL;
        Self {
            exp: exp.into_boxed_slice(),
            log: log.into_boxed_slice(),
        }
    }

    pub fn add(&self, left: u16, right: u16) -> u16 {
        left ^ right
    }

    pub fn mul(&self, left: u16, right: u16) -> u16 {
        if left == 0 || right == 0 {
            return 0;
        }
        let index = self.log[left as usize] + self.log[right as usize];
        self.exp[index as usize]
    }

    pub fn inv(&self, value: u16) -> Result<u16> {
        if value == 0 {
            return Err(Error::SingularElement);
        }
        let index = FIELD_SIZE as u32 - self.log[value as usize];
        Ok(self.exp[index as usize])
    }

    pub fn div(&self, numerator: u16, denominator: u16) -> Result<u16> {
        Ok(self.mul(numerator, self.inv(denominator)?))
    }
}

impl Default for Gf16 {
    fn default() -> Self {
        Self::new()
    }
}

pub fn make_encoder_matrix(data_shards: usize, recovery_shards: usize) -> Result<Vec<Vec<u16>>> {
    if data_shards == 0 || recovery_shards == 0 || data_shards + recovery_shards > FIELD_SIZE {
        return Err(Error::TooManyShards);
    }
    let gf = shared_gf16();
    let mut matrix = vec![vec![0u16; data_shards]; recovery_shards];
    for (i, row) in matrix.iter_mut().enumerate() {
        for (j, cell) in row.iter_mut().enumerate() {
            let denominator = ((i + data_shards) ^ j) as u16;
            *cell = gf.inv(denominator)?;
        }
    }
    Ok(matrix)
}

pub fn encode_parity_shards(data: &[&[u8]], recovery_shards: usize) -> Result<Vec<Vec<u8>>> {
    encode_parity_shards_with_progress(data, recovery_shards, |_| {})
}

pub(super) fn encode_parity_shards_with_progress(
    data: &[&[u8]],
    recovery_shards: usize,
    mut progress: impl FnMut(u64),
) -> Result<Vec<Vec<u8>>> {
    let Some(first) = data.first() else {
        return Err(Error::TooManyShards);
    };
    if !first.len().is_multiple_of(2) {
        return Err(Error::OddShardSize);
    }
    if data.iter().any(|shard| shard.len() != first.len()) {
        return Err(Error::ShardSizeMismatch);
    }

    let matrix = make_encoder_matrix(data.len(), recovery_shards)?;
    let gf = shared_gf16();
    #[cfg(feature = "parallel")]
    {
        use rayon::prelude::*;

        // Every parity row is independent; the parallel result is
        // identical to the sequential loop.
        let parity: Vec<Vec<u8>> = (0..recovery_shards)
            .into_par_iter()
            .map(|recovery_index| {
                let row = &matrix[recovery_index];
                let mut shard = vec![0u8; first.len()];
                for word_offset in (0..first.len()).step_by(2) {
                    let mut symbol = 0u16;
                    for (data_index, shard_data) in data.iter().enumerate() {
                        let data_symbol = u16::from_le_bytes([
                            shard_data[word_offset],
                            shard_data[word_offset + 1],
                        ]);
                        symbol ^= gf.mul(row[data_index], data_symbol);
                    }
                    shard[word_offset..word_offset + 2].copy_from_slice(&symbol.to_le_bytes());
                }
                shard
            })
            .collect();
        progress((recovery_shards * first.len()) as u64);
        Ok(parity)
    }
    #[cfg(not(feature = "parallel"))]
    {
        let mut parity = vec![vec![0u8; first.len()]; recovery_shards];
        for (recovery_index, row) in matrix.iter().enumerate() {
            for word_offset in (0..first.len()).step_by(2) {
                let mut symbol = 0u16;
                for (data_index, shard) in data.iter().enumerate() {
                    let data_symbol =
                        u16::from_le_bytes([shard[word_offset], shard[word_offset + 1]]);
                    symbol ^= gf.mul(row[data_index], data_symbol);
                }
                parity[recovery_index][word_offset..word_offset + 2]
                    .copy_from_slice(&symbol.to_le_bytes());
            }
            progress(((recovery_index + 1) * first.len()) as u64);
        }
        Ok(parity)
    }
}

use crate::DitError;

pub(crate) const MATRIX_WIDTH: usize = 4;
pub(crate) type Matrix = [[u8; MATRIX_WIDTH]; MATRIX_WIDTH];

#[must_use]
pub(crate) fn multiply(mut left: u8, mut right: u8) -> u8 {
    let mut result = 0_u8;
    for _ in 0..8 {
        if right & 1 != 0 {
            result ^= left;
        }
        let high_bit = left & 0x80;
        left <<= 1;
        if high_bit != 0 {
            left ^= 0x1d;
        }
        right >>= 1;
    }
    result
}

fn power(mut base: u8, mut exponent: u16) -> u8 {
    let mut result = 1_u8;
    while exponent != 0 {
        if exponent & 1 != 0 {
            result = multiply(result, base);
        }
        base = multiply(base, base);
        exponent >>= 1;
    }
    result
}

fn inverse(value: u8) -> Option<u8> {
    (value != 0).then(|| power(value, 254))
}

#[must_use]
pub(crate) fn reference_matrix() -> Matrix {
    let mut matrix = [[0_u8; MATRIX_WIDTH]; MATRIX_WIDTH];
    for (row, matrix_row) in matrix.iter_mut().enumerate() {
        let base = u8::try_from(row + 1).expect("matrix width fits u8");
        let mut value = 1_u8;
        for cell in matrix_row {
            *cell = value;
            value = multiply(value, base);
        }
    }
    matrix
}

pub(crate) fn invert(matrix: &Matrix) -> Result<Matrix, DitError> {
    let mut augmented = [[0_u8; MATRIX_WIDTH * 2]; MATRIX_WIDTH];
    for row in 0..MATRIX_WIDTH {
        for column in 0..MATRIX_WIDTH {
            augmented[row][column] = matrix[row][column];
        }
        augmented[row][MATRIX_WIDTH + row] = 1;
    }

    for pivot_column in 0..MATRIX_WIDTH {
        let pivot_row = (pivot_column..MATRIX_WIDTH)
            .find(|&row| augmented[row][pivot_column] != 0)
            .ok_or(DitError::MatrixNotInvertible)?;
        augmented.swap(pivot_column, pivot_row);

        let pivot_inverse =
            inverse(augmented[pivot_column][pivot_column]).ok_or(DitError::MatrixNotInvertible)?;
        for cell in &mut augmented[pivot_column] {
            *cell = multiply(*cell, pivot_inverse);
        }

        for row in 0..MATRIX_WIDTH {
            if row == pivot_column {
                continue;
            }
            let factor = augmented[row][pivot_column];
            if factor == 0 {
                continue;
            }
            let pivot = augmented[pivot_column];
            for (cell, pivot_cell) in augmented[row].iter_mut().zip(pivot) {
                *cell ^= multiply(factor, pivot_cell);
            }
        }
    }

    let mut result = [[0_u8; MATRIX_WIDTH]; MATRIX_WIDTH];
    for row in 0..MATRIX_WIDTH {
        for column in 0..MATRIX_WIDTH {
            result[row][column] = augmented[row][MATRIX_WIDTH + column];
        }
    }
    Ok(result)
}

pub(crate) fn apply(
    matrix: &Matrix,
    input: &[Vec<u8>; MATRIX_WIDTH],
) -> Result<[Vec<u8>; MATRIX_WIDTH], DitError> {
    let block_size = input[0].len();
    if input.iter().any(|block| block.len() != block_size) {
        return Err(DitError::InvalidConfig(
            "matrix input blocks must have equal lengths".to_owned(),
        ));
    }

    let mut output = std::array::from_fn(|_| zeroed(block_size));
    for row in 0..MATRIX_WIDTH {
        for column in 0..MATRIX_WIDTH {
            let coefficient = matrix[row][column];
            for offset in 0..block_size {
                output[row][offset] ^= multiply(coefficient, input[column][offset]);
            }
        }
    }
    Ok(output)
}

fn zeroed(bytes: usize) -> Vec<u8> {
    vec![0_u8; bytes]
}

#[cfg(test)]
mod tests {
    use super::{MATRIX_WIDTH, apply, invert, multiply, reference_matrix};

    #[test]
    fn gf_multiplication_has_identity() {
        for value in u8::MIN..=u8::MAX {
            assert_eq!(multiply(value, 1), value);
            assert_eq!(multiply(value, 0), 0);
        }
    }

    #[test]
    fn reference_matrix_round_trips() {
        let matrix = reference_matrix();
        let inverse = invert(&matrix).expect("reference matrix must be invertible");
        let input: [Vec<u8>; MATRIX_WIDTH] = [
            vec![0, 1, 2, 3, 4],
            vec![5, 6, 7, 8, 9],
            vec![10, 11, 12, 13, 14],
            vec![15, 16, 17, 18, 19],
        ];
        let encoded = apply(&matrix, &input).expect("encode");
        let decoded = apply(&inverse, &encoded).expect("decode");
        assert_eq!(decoded, input);
    }
}

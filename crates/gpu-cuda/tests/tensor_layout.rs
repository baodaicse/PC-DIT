//! CPU emulation of `dit_gemm_i8` (kernels/tensor.cu): every lane's loads are placed into the
//! 16x32 A and 32x8 B tiles by the PTX ISA fragment layout of `mma.m16n8k32` for `.s8`, the tile
//! product is taken, and each lane's four results go where the kernel stores them. The emulation
//! must reproduce the INT8 oracle, proving the kernel's addressing agrees with that layout. It
//! cannot prove the layout itself; that needs tensor-core hardware (compute capability 8.0+).
// g/t/n/a/d follow the PTX ISA fragment notation and the kernel's own names.
#![allow(clippy::many_single_char_names)]
use gpu_engine::gemm::{Arith, Corpus};

#[test]
fn the_tensor_kernel_addressing_reproduces_the_int8_oracle() {
    let corpus = Corpus::new_arith(Arith::I8, 4, 2, 96, 1).unwrap();
    let (n, inner) = (corpus.n, corpus.k);
    let words = inner / 4;
    for job in 0..corpus.jobs {
        let a = &corpus.input[job * 2 * n * words..];
        let bt = &a[n * words..];
        let mut d = vec![0_i32; n * n];
        for tile in 0..(n / 16) * (n / 8) {
            let (row0, col0) = ((tile / (n / 8)) * 16, (tile % (n / 8)) * 8);
            let mut acc = [[0_i32; 8]; 16];
            for k in (0..inner).step_by(32) {
                let kw = k / 4;
                let (mut at, mut bm) = ([[0_i32; 32]; 16], [[0_i32; 8]; 32]);
                for lane in 0..32 {
                    let (g, t) = (lane >> 2, lane & 3);
                    // The kernel's loads, lane by lane.
                    let regs_a = [
                        a[(row0 + g) * words + kw + t],
                        a[(row0 + g + 8) * words + kw + t],
                        a[(row0 + g) * words + kw + 4 + t],
                        a[(row0 + g + 8) * words + kw + 4 + t],
                    ];
                    let regs_b = [
                        bt[(col0 + g) * words + kw + t],
                        bt[(col0 + g) * words + kw + 4 + t],
                    ];
                    // PTX ISA layout: element i of A sits in register i/4, byte i%4.
                    for i in 0..16 {
                        let row = if i < 4 || (8..12).contains(&i) {
                            g
                        } else {
                            g + 8
                        };
                        let col = t * 4 + (i & 3) + if i >= 8 { 16 } else { 0 };
                        let byte = regs_a[i / 4].to_le_bytes()[i % 4].cast_signed();
                        at[row][col] = i32::from(byte);
                    }
                    for i in 0..8 {
                        let row = t * 4 + (i & 3) + if i >= 4 { 16 } else { 0 };
                        let byte = regs_b[i / 4].to_le_bytes()[i % 4].cast_signed();
                        bm[row][g] = i32::from(byte);
                    }
                }
                for (r, out) in acc.iter_mut().enumerate() {
                    for (c, cell) in out.iter_mut().enumerate() {
                        *cell += (0..32).map(|x| at[r][x] * bm[x][c]).sum::<i32>();
                    }
                }
            }
            for lane in 0..32 {
                let (g, t) = (lane >> 2, lane & 3);
                // D layout: d0,d1 = row g, cols 2t, 2t+1; d2,d3 = row g+8, same columns.
                for (r, c) in [
                    (g, 2 * t),
                    (g, 2 * t + 1),
                    (g + 8, 2 * t),
                    (g + 8, 2 * t + 1),
                ] {
                    d[(row0 + r) * n + col0 + c] = acc[r][c];
                }
            }
        }
        let oracle = &corpus.dot[job * n * n..(job + 1) * n * n];
        let emulated: Vec<u32> = d.iter().map(|v| v.cast_unsigned()).collect();
        assert_eq!(emulated, oracle, "job {job}");
    }
}

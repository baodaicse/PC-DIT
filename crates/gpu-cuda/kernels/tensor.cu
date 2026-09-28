// INT8 tensor-core GEMM (compute capability 8.0+, built for compute_80, loaded only there).
//
// Per job: A is n x k signed bytes, row-major; B is given transposed (n x k, row-major, i.e. B
// column-major), both packed four bytes to a word. D = A * B with 32-bit integer accumulation.
// |a*b| <= 2^14 and k <= 4096, so no sum reaches 2^31: the result is exact, and it equals
// gpu_engine::gemm's INT8 oracle bit for bit. n must be a multiple of 16 and k of 32.
//
// One warp computes one 16x8 tile of D with mma.sync.m16n8k32.s8.s8.s32, fragments loaded
// straight from global memory in the PTX ISA layout (groupID = lane / 4, t = lane % 4):
//   A: a0 = row g, k t*4..+3;  a1 = row g+8, same k;  a2 = row g, k 16+t*4..;  a3 = row g+8, same
//   B: b0 = column g, k t*4..+3;  b1 = column g, k 16+t*4..
//   D: d0,d1 = row g, columns 2t, 2t+1;  d2,d3 = row g+8, same columns
// Output layout, tagging and fault modes are those of dit_gemm.

__device__ __forceinline__ void store_cell(unsigned *output, unsigned out, unsigned n,
    unsigned job, unsigned row, unsigned col, int sum, unsigned lo, unsigned hi, unsigned fault) {
    unsigned bits = (unsigned)sum;
    if (fault == 1 && row == 0 && col == 0) bits ^= 1;
    if (fault == 3 && row == 0 && col == 0) return;
    if (fault == 4) lo ^= 1;
    unsigned value = bits ^ lo ^ ((hi << 13) | (hi >> 19));
    output[out + 2 + row * n + col] = ((value << 7) | (value >> 25)) ^ (job * 0x9E3779B9u) ^ (row * n + col);
    if (row == 0 && col == 0) { output[out] = lo; output[out + 1] = hi; }
    if (fault == 5 && row == 0 && col == 0) output[out + 2 + n * n] = 0;
}

// Block: 4 warps (128 threads); grid: (n/16 * n/8 / 4, 1, jobs).
extern "C" __global__ void dit_gemm_i8(const unsigned *input, unsigned *output,
    unsigned jobs, unsigned inner, unsigned n, unsigned lo, unsigned hi, unsigned fault) {
    unsigned lane = threadIdx.x & 31, warp = threadIdx.x >> 5;
    unsigned g = lane >> 2, t = lane & 3;
    unsigned col_tiles = n / 8;
    unsigned tile = blockIdx.x * 4 + warp;
    if (tile >= (n / 16) * col_tiles) return;
    unsigned row0 = (tile / col_tiles) * 16, col0 = (tile % col_tiles) * 8;
    unsigned job = blockIdx.z;
    unsigned words = inner / 4;                  // words per row of A or of B^T
    const unsigned *a = input + (size_t)job * 2 * n * words;
    const unsigned *bt = a + (size_t)n * words;
    unsigned arow = (fault == 2) ? (row0 + g + 1) % n : row0 + g;
    unsigned arow8 = (fault == 2) ? (row0 + g + 9) % n : row0 + g + 8;
    int d0 = 0, d1 = 0, d2 = 0, d3 = 0;
    for (unsigned k = 0; k < inner; k += 32) {
        unsigned kw = k / 4;
        unsigned a0 = a[arow * words + kw + t];
        unsigned a1 = a[arow8 * words + kw + t];
        unsigned a2 = a[arow * words + kw + 4 + t];
        unsigned a3 = a[arow8 * words + kw + 4 + t];
        unsigned b0 = bt[(col0 + g) * words + kw + t];
        unsigned b1 = bt[(col0 + g) * words + kw + 4 + t];
        asm volatile(
            "mma.sync.aligned.m16n8k32.row.col.s32.s8.s8.s32 "
            "{%0,%1,%2,%3}, {%4,%5,%6,%7}, {%8,%9}, {%0,%1,%2,%3};\n"
            : "+r"(d0), "+r"(d1), "+r"(d2), "+r"(d3)
            : "r"(a0), "r"(a1), "r"(a2), "r"(a3), "r"(b0), "r"(b1));
    }
    unsigned out = job * (n * n + 4);
    store_cell(output, out, n, job, row0 + g, col0 + 2 * t, d0, lo, hi, fault);
    store_cell(output, out, n, job, row0 + g, col0 + 2 * t + 1, d1, lo, hi, fault);
    store_cell(output, out, n, job, row0 + g + 8, col0 + 2 * t, d2, lo, hi, fault);
    store_cell(output, out, n, job, row0 + g + 8, col0 + 2 * t + 1, d3, lo, hi, fault);
}

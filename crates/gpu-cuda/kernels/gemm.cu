extern "C" __global__ void dit_gemm(const unsigned *input, unsigned *output,
    unsigned jobs, unsigned inner, unsigned n, unsigned lo, unsigned hi, unsigned fault) {
    __shared__ unsigned left[16][16], right[16][16];
    unsigned x=threadIdx.x, y=threadIdx.y;
    unsigned row=blockIdx.y*16+y, col=blockIdx.x*16+x;
    unsigned job=blockIdx.z, base=job*2*n*inner, sum=0;
    for(unsigned k=0;k<inner;k+=16) {
        unsigned source_row=(fault==2)?(row+1)%n:row;
        left[y][x]=(row<n && k+x<inner)?input[base+source_row*inner+k+x]:0;
        right[y][x]=(col<n && k+y<inner)?input[base+n*inner+(k+y)*n+col]:0;
        __syncthreads();
        #pragma unroll
        for(unsigned t=0;t<16;++t) sum+=left[y][t]*right[t][x];
        __syncthreads();
    }
    unsigned out=job*(n*n+4);
    if(row<n && col<n) {
        if(fault==1 && row==0 && col==0) sum^=1;
        if(fault==3 && row==0 && col==0) return;
        if(fault==4) lo^=1;
        unsigned value=sum ^ lo ^ ((hi<<13)|(hi>>19));
        output[out+2+row*n+col]=((value<<7)|(value>>25)) ^ (job*0x9E3779B9u) ^ (row*n+col);
        if(row==0 && col==0) { output[out]=lo; output[out+1]=hi; }
        if(fault==5 && row==0 && col==0) output[out+2+n*n]=0;
    }
}

// FP32 GEMM on the FMA pipes: the same layout, tagging and fault modes as dit_gemm, with every
// product accumulated by one fused multiply-add in ascending k (each thread walks its tiles and
// their columns in order), which is exactly what gpu_engine::gemm's FP32 oracle computes.
extern "C" __global__ void dit_gemm_f32(const unsigned *input, unsigned *output,
    unsigned jobs, unsigned inner, unsigned n, unsigned lo, unsigned hi, unsigned fault) {
    __shared__ float left[16][16], right[16][16];
    unsigned x=threadIdx.x, y=threadIdx.y;
    unsigned row=blockIdx.y*16+y, col=blockIdx.x*16+x;
    unsigned job=blockIdx.z, base=job*2*n*inner;
    float sum=0.0f;
    for(unsigned k=0;k<inner;k+=16) {
        unsigned source_row=(fault==2)?(row+1)%n:row;
        left[y][x]=(row<n && k+x<inner)?__uint_as_float(input[base+source_row*inner+k+x]):0.0f;
        right[y][x]=(col<n && k+y<inner)?__uint_as_float(input[base+n*inner+(k+y)*n+col]):0.0f;
        __syncthreads();
        #pragma unroll
        for(unsigned t=0;t<16;++t) sum=__fmaf_rn(left[y][t], right[t][x], sum);
        __syncthreads();
    }
    unsigned out=job*(n*n+4);
    if(row<n && col<n) {
        unsigned bits=__float_as_uint(sum);
        if(fault==1 && row==0 && col==0) bits^=1;
        if(fault==3 && row==0 && col==0) return;
        if(fault==4) lo^=1;
        unsigned value=bits ^ lo ^ ((hi<<13)|(hi>>19));
        output[out+2+row*n+col]=((value<<7)|(value>>25)) ^ (job*0x9E3779B9u) ^ (row*n+col);
        if(row==0 && col==0) { output[out]=lo; output[out+1]=hi; }
        if(fault==5 && row==0 && col==0) output[out+2+n*n]=0;
    }
}

// GPU-H6 GPU-local VRAM workload. `vram_word` must equal gpu_engine::lane::vram_word.
__device__ __forceinline__ unsigned vram_word(unsigned long long index, unsigned key) {
    unsigned x = ((unsigned)index) * 0x9E3779B1u ^ (key * 0x85EBCA6Bu)
        ^ (((unsigned)(index >> 32)) * 0xC2B2AE35u);
    x ^= x >> 16; x *= 0x7FEB352Du; x ^= x >> 15; x *= 0x846CA68Bu;
    return x ^ (x >> 16);
}

// Writes words [first, first+words) of an allocation from the device side (no PCIe).
extern "C" __global__ void dit_vram_fill(unsigned *region, unsigned long long first,
    unsigned long long words, unsigned key) {
    unsigned long long stride = (unsigned long long)gridDim.x * blockDim.x;
    for (unsigned long long i = (unsigned long long)blockIdx.x * blockDim.x + threadIdx.x;
         i < words; i += stride)
        region[i] = vram_word(first + i, key);
}

// Reads the region back on the device and counts words that differ from the pattern.
extern "C" __global__ void dit_vram_verify(const unsigned *region, unsigned long long first,
    unsigned long long words, unsigned key, unsigned *bad) {
    unsigned long long stride = (unsigned long long)gridDim.x * blockDim.x;
    unsigned local = 0;
    for (unsigned long long i = (unsigned long long)blockIdx.x * blockDim.x + threadIdx.x;
         i < words; i += stride)
        local += region[i] != vram_word(first + i, key);
    if (local) atomicAdd(bad, local);
}

// GPU-DRAM sweep word. Must equal gpu_engine::sweep::sweep_word. No multiply: at DRAM bandwidth a
// multiply-heavy pattern makes the sweep compete with the GEMM for ALU instead of memory. For a
// fixed key it is a bijection of the low 32 index bits (xor, then xorshift32), so no two words
// of an allocation below 16 GiB hold the same value and an address fault cannot pass.
__device__ __forceinline__ unsigned sweep_word(unsigned long long index, unsigned key) {
    unsigned hi = (unsigned)(index >> 32);
    unsigned x = ((unsigned)index) ^ key ^ ((hi << 16) | (hi >> 16));
    x ^= x << 13; x ^= x >> 17; x ^= x << 5;
    return x;
}

// GPU-DRAM sweep: 16-byte accesses over `vectors` uint4 (4 words each) starting at word `first`
// of the allocation; `region` points at that word.
extern "C" __global__ void dit_vram_sweep_fill(uint4 *region, unsigned long long first,
    unsigned long long vectors, unsigned key) {
    unsigned long long stride = (unsigned long long)gridDim.x * blockDim.x;
    for (unsigned long long v = (unsigned long long)blockIdx.x * blockDim.x + threadIdx.x;
         v < vectors; v += stride) {
        unsigned long long w = first + v * 4;
        region[v] = make_uint4(sweep_word(w, key), sweep_word(w + 1, key),
            sweep_word(w + 2, key), sweep_word(w + 3, key));
    }
}

// Reads a swept span back on the device and counts words that differ from the pattern.
extern "C" __global__ void dit_vram_sweep_verify(const uint4 *region, unsigned long long first,
    unsigned long long vectors, unsigned key, unsigned *bad) {
    unsigned long long stride = (unsigned long long)gridDim.x * blockDim.x;
    unsigned local = 0;
    for (unsigned long long v = (unsigned long long)blockIdx.x * blockDim.x + threadIdx.x;
         v < vectors; v += stride) {
        uint4 x = region[v];
        unsigned long long w = first + v * 4;
        local += (x.x != sweep_word(w, key)) + (x.y != sweep_word(w + 1, key))
            + (x.z != sweep_word(w + 2, key)) + (x.w != sweep_word(w + 3, key));
    }
    if (local) atomicAdd(bad, local);
}

// ---- Boundary checks (docs/COMPONENT_FAULT_ATTRIBUTION_ROADMAP.md, step F3) ----
// `boundary_mix` must equal gpu_engine::gemm::boundary_mix: the hash of a word array is the sum
// (mod 2^32) of boundary_mix(index, word), so blocks can add their parts in any order. The word
// is XORed with a key of its position (32-bit multiplies only: cheap on every GPU); a change of
// one word always changes the sum (the difference is non-zero mod 2^32), and so does moving
// words to other positions.
__device__ __forceinline__ unsigned boundary_mix(unsigned long long index, unsigned word) {
    return word ^ ((unsigned)index * 0x9E3779B1u) ^ ((unsigned)(index >> 32) * 0x85EBCA6Bu);
}

// Sum of `value` over the block, valid in thread 0. `scratch` holds one word per warp; a second
// call needs another scratch array (or a barrier in between).
__device__ __forceinline__ unsigned block_sum(unsigned value, unsigned *scratch) {
    for (int offset = 16; offset > 0; offset >>= 1)
        value += __shfl_down_sync(0xffffffffu, value, offset);
    unsigned tid = threadIdx.y * blockDim.x + threadIdx.x;
    unsigned warps = (blockDim.x * blockDim.y + 31) >> 5;
    if ((tid & 31) == 0) scratch[tid >> 5] = value;
    __syncthreads();
    if (tid == 0)
        for (unsigned w = 1; w < warps; ++w) value += scratch[w];
    return value;
}

// Hash of `count` words (the corpus input, the answers): adds into out[0], zeroed by the caller.
extern "C" __global__ void dit_hash_words(const unsigned *words, unsigned long long count,
    unsigned *out) {
    unsigned long long stride = (unsigned long long)gridDim.x * blockDim.x;
    unsigned local = 0;
    for (unsigned long long i = (unsigned long long)blockIdx.x * blockDim.x + threadIdx.x;
         i < count; i += stride)
        local += boundary_mix(i, words[i]);
    if (local) atomicAdd(out, local);
}

// Separate check for the INT8 tensor GEMM (the u32 and FP32 GEMMs have it fused in): the GPU
// checks its own output before the copy back -- every word of every job against what it must be
// (generation words, tagged answer, guards; the rule of Corpus::verify_bytes) -- and hashes it.
// One block per job (blockIdx.x); each writes its partial {bad, 0, hash, 0} to partials[job].
extern "C" __global__ void dit_check_output(const unsigned *output, const unsigned *answers,
    unsigned jobs, unsigned n, unsigned lo, unsigned hi, unsigned *partials) {
    __shared__ unsigned bad_part[32], hash_part[32];
    unsigned cells = n * n, stride_words = cells + 4, job = blockIdx.x;
    unsigned epoch = lo ^ ((hi << 13) | (hi >> 19));
    unsigned job_tag = job * 0x9E3779B9u;
    unsigned long long base = (unsigned long long)job * stride_words;
    const unsigned *out = output + base, *dots = answers + (unsigned long long)job * cells;
    unsigned bad = 0, hash = 0;
    for (unsigned cell = threadIdx.x; cell < stride_words; cell += blockDim.x) {
        unsigned expected;
        if (cell == 0) expected = lo;
        else if (cell == 1) expected = hi;
        else if (cell >= stride_words - 2) expected = 0xA55A1973u;
        else {
            unsigned c = cell - 2, value = dots[c] ^ epoch;
            expected = ((value << 7) | (value >> 25)) ^ job_tag ^ c;
        }
        unsigned word = out[cell];
        bad += word != expected;
        hash += boundary_mix(base + cell, word);
    }
    bad = block_sum(bad, bad_part);
    hash = block_sum(hash, hash_part);
    if (threadIdx.x == 0 && job < jobs) {
        unsigned *mine = partials + 4ull * job;
        mine[0] = bad; mine[1] = 0; mine[2] = hash; mine[3] = 0;
    }
}

// Sums `blocks` partials into the tail {bad, seal, hash, 0} and seals it, so a stale or damaged
// tail is never read as a verdict. `fault` 1 (test only): change one output word after the
// check, as a copy back would. Runs in one block.
__device__ __forceinline__ void boundary_seal(unsigned *output, unsigned lo, unsigned *tail,
    const volatile unsigned *partials, unsigned blocks, unsigned fault) {
    __shared__ unsigned bad_part[32], hash_part[32];
    unsigned tid = threadIdx.y * blockDim.x + threadIdx.x, threads = blockDim.x * blockDim.y;
    unsigned bad = 0, hash = 0;
    for (unsigned b = tid; b < blocks; b += threads) {
        bad += partials[4ull * b];
        hash += partials[4ull * b + 2];
    }
    bad = block_sum(bad, bad_part);
    hash = block_sum(hash, hash_part);
    if (tid == 0) {
        if (fault == 1) output[2] ^= 0x10u;
        tail[0] = bad; tail[2] = hash; tail[3] = 0;
        tail[1] = 0xB0DA7A11u ^ lo ^ bad ^ hash;
    }
}

extern "C" __global__ void dit_check_seal(unsigned *output, unsigned lo, unsigned *tail,
    const unsigned *partials, unsigned jobs, unsigned fault) {
    boundary_seal(output, lo, tail, partials, jobs, fault);
}

// ---- GEMM with the boundary check fused in (step F3) ----
// The same GEMM as dit_gemm / dit_gemm_f32 (which stay untouched for runs without the checks),
// followed by the GPU's check of what it wrote, so a batch needs no extra launch:
//   1. every thread reads back the word it wrote and compares it with the tagged answer, and
//      adds it to the output hash; the first block of each job also checks the generation words
//      and the two guards;
//   2. each block writes its partial {bad, 0, hash, 0} to partials[block];
//   3. the last block to finish (ticket from *counter) sums the partials, seals the tail and
//      resets *counter to 0 for the next batch of this slot (zeroed once at allocation).
// `seal_fault` 1 (test only): change one output word after the check, as a copy back would.
__device__ __forceinline__ unsigned boundary_expected(const unsigned *answers, unsigned job,
    unsigned cells, unsigned c, unsigned epoch) {
    unsigned value = answers[(unsigned long long)job * cells + c] ^ epoch;
    return ((value << 7) | (value >> 25)) ^ (job * 0x9E3779B9u) ^ c;
}

__device__ void boundary_check_tile(unsigned *output, const unsigned *answers,
    unsigned n, unsigned lo, unsigned hi, unsigned row, unsigned col, unsigned job,
    unsigned *tail, unsigned *partials, unsigned *counter, unsigned seal_fault) {
    __shared__ unsigned hash_part[32];
    __shared__ unsigned last;
    unsigned cells = n * n, out = job * (cells + 4);
    unsigned epoch = lo ^ ((hi << 13) | (hi >> 19));
    unsigned tid = threadIdx.y * blockDim.x + threadIdx.x;
    const volatile unsigned *written = output;
    unsigned mismatch = 0, hash = 0;
    if (row < n && col < n) {
        unsigned c = row * n + col;
        unsigned word = written[out + 2 + c];
        mismatch = word != boundary_expected(answers, job, cells, c, epoch);
        hash = boundary_mix(out + 2 + c, word);
    }
    // One barrier that also counts: after it every write of the block (header, guard) is visible.
    unsigned bad = __syncthreads_count(mismatch);
    if (blockIdx.x == 0 && blockIdx.y == 0) {
        unsigned header = 0;
        if (tid < 4) {
            unsigned index = tid < 2 ? out + tid : out + cells + tid;
            unsigned expected = tid == 0 ? lo : tid == 1 ? hi : 0xA55A1973u;
            unsigned word = written[index];
            header = word != expected;
            hash += boundary_mix(index, word);
        }
        bad += __syncthreads_count(header);
    }
    hash = block_sum(hash, hash_part);
    unsigned blocks = gridDim.x * gridDim.y * gridDim.z;
    if (tid == 0) {
        unsigned block = (blockIdx.z * gridDim.y + blockIdx.y) * gridDim.x + blockIdx.x;
        unsigned *mine = partials + 4ull * block;
        mine[0] = bad; mine[1] = 0; mine[2] = hash; mine[3] = 0;
        __threadfence();
        last = atomicAdd(counter, 1u) == blocks - 1;
    }
    __syncthreads();
    if (!last) return;
    __threadfence();
    boundary_seal(output, lo, tail, partials, blocks, seal_fault);
    if (tid == 0) *counter = 0;
}

extern "C" __global__ void dit_gemm_checked(const unsigned *input, unsigned *output,
    unsigned jobs, unsigned inner, unsigned n, unsigned lo, unsigned hi, unsigned fault,
    const unsigned *answers, unsigned *tail, unsigned *partials, unsigned *counter,
    unsigned seal_fault) {
    __shared__ unsigned left[16][16], right[16][16];
    unsigned x=threadIdx.x, y=threadIdx.y;
    unsigned row=blockIdx.y*16+y, col=blockIdx.x*16+x;
    unsigned job=blockIdx.z, base=job*2*n*inner, sum=0;
    unsigned tag_lo = lo;
    for(unsigned k=0;k<inner;k+=16) {
        unsigned source_row=(fault==2)?(row+1)%n:row;
        left[y][x]=(row<n && k+x<inner)?input[base+source_row*inner+k+x]:0;
        right[y][x]=(col<n && k+y<inner)?input[base+n*inner+(k+y)*n+col]:0;
        __syncthreads();
        #pragma unroll
        for(unsigned t=0;t<16;++t) sum+=left[y][t]*right[t][x];
        __syncthreads();
    }
    unsigned out=job*(n*n+4);
    if(row<n && col<n) {
        if(fault==1 && row==0 && col==0) sum^=1;
        if(fault==4) tag_lo^=1;
        unsigned value=sum ^ tag_lo ^ ((hi<<13)|(hi>>19));
        if(!(fault==3 && row==0 && col==0))
            output[out+2+row*n+col]=((value<<7)|(value>>25)) ^ (job*0x9E3779B9u) ^ (row*n+col);
        if(row==0 && col==0) { output[out]=tag_lo; output[out+1]=hi; }
        if(fault==5 && row==0 && col==0) output[out+2+n*n]=0;
    }
    boundary_check_tile(output, answers, n, lo, hi, row, col, job, tail, partials, counter,
        seal_fault);
}

extern "C" __global__ void dit_gemm_f32_checked(const unsigned *input, unsigned *output,
    unsigned jobs, unsigned inner, unsigned n, unsigned lo, unsigned hi, unsigned fault,
    const unsigned *answers, unsigned *tail, unsigned *partials, unsigned *counter,
    unsigned seal_fault) {
    __shared__ float left[16][16], right[16][16];
    unsigned x=threadIdx.x, y=threadIdx.y;
    unsigned row=blockIdx.y*16+y, col=blockIdx.x*16+x;
    unsigned job=blockIdx.z, base=job*2*n*inner;
    unsigned tag_lo = lo;
    float sum=0.0f;
    for(unsigned k=0;k<inner;k+=16) {
        unsigned source_row=(fault==2)?(row+1)%n:row;
        left[y][x]=(row<n && k+x<inner)?__uint_as_float(input[base+source_row*inner+k+x]):0.0f;
        right[y][x]=(col<n && k+y<inner)?__uint_as_float(input[base+n*inner+(k+y)*n+col]):0.0f;
        __syncthreads();
        #pragma unroll
        for(unsigned t=0;t<16;++t) sum=__fmaf_rn(left[y][t], right[t][x], sum);
        __syncthreads();
    }
    unsigned out=job*(n*n+4);
    if(row<n && col<n) {
        unsigned bits=__float_as_uint(sum);
        if(fault==1 && row==0 && col==0) bits^=1;
        if(fault==4) tag_lo^=1;
        unsigned value=bits ^ tag_lo ^ ((hi<<13)|(hi>>19));
        if(!(fault==3 && row==0 && col==0))
            output[out+2+row*n+col]=((value<<7)|(value>>25)) ^ (job*0x9E3779B9u) ^ (row*n+col);
        if(row==0 && col==0) { output[out]=tag_lo; output[out+1]=hi; }
        if(fault==5 && row==0 && col==0) output[out+2+n*n]=0;
    }
    boundary_check_tile(output, answers, n, lo, hi, row, col, job, tail, partials, counter,
        seal_fault);
}

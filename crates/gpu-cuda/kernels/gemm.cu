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

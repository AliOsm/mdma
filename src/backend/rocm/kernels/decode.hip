// T=1 K-quant matvec against quantize_q8_1_kernel's {q8, float2(d, sum)}.
// One wave per row, two packed four-element dot products per lane/SB.

__device__ __forceinline__ uint32_t decode_load4(const void* p) {
    uint32_t v;
    memcpy(&v, p, sizeof(v)); // Q6_K superblocks are only two-byte aligned.
    return v;
}

__device__ __forceinline__ int decode_dot4(uint32_t a, uint32_t b) {
#if defined(__gfx1100__) || defined(__gfx1101__) || defined(__gfx1102__) || defined(__gfx1200__) || defined(__gfx1201__)
    return __builtin_amdgcn_sudot4(true, (int)a, true, (int)b, 0, false);
#else
    return __builtin_amdgcn_sdot4((int)a, (int)b, 0, false);
#endif
}

// Two superblocks per warp iteration: 16 lanes per superblock, 8 packed
// bytes (four dot4) per lane.
// Lane l: superblock parity l / 16, byte pair j = l % 16 -> group j / 4
// (64 K), 8 bytes at offset (j % 4) * 8 within the group's 32 packed
// bytes: low nibbles K = g*64 + (j%4)*8 + 0..7, high nibbles +32.
struct Q4kRaw2 {
    uint32_t p0, p1, xa0, xa1, xb0, xb1;
    float2 y0, y1;
    uint32_t hdr[4];
};

template<bool Prepared>
__device__ __forceinline__ Q4kRaw2 decode_q4k2_load(const uint8_t* block, const int8_t* x, const float2* ys, int j) {
    const int group = j / 4;
    const int offset = (j % 4) * 8;
    Q4kRaw2 r{};
    memcpy(r.hdr, block, 16);
    constexpr int quants = Prepared ? Q4K_PACKED_QUANTS_OFFSET : 16;
    r.p0 = decode_load4(block + quants + group * 32 + offset);
    r.p1 = decode_load4(block + quants + group * 32 + offset + 4);
    r.xa0 = decode_load4(x + group * 64 + offset);
    r.xa1 = decode_load4(x + group * 64 + offset + 4);
    r.xb0 = decode_load4(x + group * 64 + offset + 32);
    r.xb1 = decode_load4(x + group * 64 + offset + 36);
    r.y0 = ys[group * 2];
    r.y1 = ys[group * 2 + 1];
    return r;
}

template<bool Prepared>
__device__ __forceinline__ float decode_q4k2_eval(const Q4kRaw2& r, int j) {
    #pragma clang fp contract(off)
    const int group = j / 4;
    const int offset = (j % 4) * 8;
    int sc0, sc1, m0, m1;
    if constexpr (Prepared) {
        const int shift = group * 8;
        const uint32_t sc = r.hdr[1] >> shift;
        const uint32_t mn = r.hdr[2] >> shift;
        const uint32_t hi = r.hdr[3] >> shift;
        sc0 = (sc & 15) | ((hi & 3) << 4);
        sc1 = ((sc >> 4) & 15) | (((hi >> 2) & 3) << 4);
        m0 = (mn & 15) | (((hi >> 4) & 3) << 4);
        m1 = ((mn >> 4) & 15) | (((hi >> 6) & 3) << 4);
    } else {
        const int shift = (group & 1) * 16;
        const uint32_t s = r.hdr[1] >> shift;
        const uint32_t m = r.hdr[2] >> shift;
        const uint32_t h = r.hdr[3] >> shift;
        const uint32_t sc = group < 2 ? s & 0x3f3fu : (h & 0x0f0fu) | ((s >> 2) & 0x3030u);
        const uint32_t mn = group < 2 ? m & 0x3f3fu : ((h >> 4) & 0x0f0fu) | ((m >> 2) & 0x3030u);
        sc0 = sc & 0xff;
        sc1 = sc >> 8;
        m0 = mn & 0xff;
        m1 = mn >> 8;
    }
    const unsigned short dbits = r.hdr[0] & 0xffffu, mbits = r.hdr[0] >> 16;
    __half dh, mh;
    memcpy(&dh, &dbits, 2);
    memcpy(&mh, &mbits, 2);
    const int lo = decode_dot4(r.p0 & 0x0f0f0f0fu, r.xa0) + decode_dot4(r.p1 & 0x0f0f0f0fu, r.xa1);
    const int hi = decode_dot4((r.p0 >> 4) & 0x0f0f0f0fu, r.xb0) + decode_dot4((r.p1 >> 4) & 0x0f0f0f0fu, r.xb1);
    // Match the raw kernel's rounding order regardless of header-load lowering.
    const float dots = fmaf(sc0 * r.y0.x, (float)lo, sc1 * r.y1.x * hi);
    // One lane per 32-element group owns its min correction.
    const float mins = offset == 0 ? fmaf(m0 * r.y0.x, r.y0.y, m1 * r.y1.x * r.y1.y) : 0.0f;
    return fmaf(__half2float(dh), dots, -__half2float(mh) * mins);
}

__device__ __forceinline__ float decode_q6k(
    const uint8_t* block, const int8_t* x, const float2* ys, int lane
) {
    const int half = lane / 16;
    const int offset = (lane % 16) * 4;
    const int k = half * 128 + offset;
    const uint32_t lo = decode_load4(block + half * 64 + offset);
    const uint32_t hi = decode_load4(block + 128 + half * 32 + offset % 32);
    const int shift = (offset / 32) * 2;
    uint32_t q0 = (lo & 0x0f0f0f0fu) | (((hi >> shift) & 0x03030303u) << 4);
    uint32_t q1 = ((lo >> 4) & 0x0f0f0f0fu) | (((hi >> (shift + 4)) & 0x03030303u) << 4);
    // Independent byte recentering from [0, 63] to [-32, 31], no carries.
    q0 = (q0 + 0x60606060u) ^ 0x80808080u;
    q1 = (q1 + 0x60606060u) ^ 0x80808080u;
    const int8_t* scales = reinterpret_cast<const int8_t*>(block + 192);
    const float d0 = ys[k / 32].x, d1 = ys[k / 32 + 2].x;
    __half dh;
    memcpy(&dh, block + 208, 2);
    return __half2float(dh) * (
        scales[k / 16] * d0 * decode_dot4(q0, decode_load4(x + k))
      + scales[k / 16 + 4] * d1 * decode_dot4(q1, decode_load4(x + k + 64)));
}

template<bool Q6, bool Gated = false, bool Prepared = false>
__device__ __forceinline__ void decode_gemv_row(
    const uint8_t* w, int row, int n_out, int n_in, const int8_t* x, const float2* ys, float* y,
    const uint8_t* up = nullptr
) {
    const int lane = threadIdx.x & 31;
    const int blocks = n_in / QK_K;
    constexpr int bytes = Q6 ? BLOCK_Q6K_BYTES : (Prepared ? BLOCK_Q4K_PACKED_BYTES : BLOCK_Q4K_BYTES);
    float acc = 0.0f;
    float up_acc = 0.0f;
    if (row < n_out) {
        const uint8_t* wr = w + (size_t)row * blocks * bytes;
        const uint8_t* ur = Gated ? up + (size_t)row * blocks * bytes : nullptr;
        if constexpr (Q6) {
            for (int b = 0; b < blocks; ++b) {
                acc += decode_q6k(wr + b * bytes, x + b * QK_K, ys + b * 8, lane);
                if constexpr (Gated) up_acc += decode_q6k(ur + b * bytes, x + b * QK_K, ys + b * 8, lane);
            }
        } else {
            // Each half-warp takes one superblock per step, G steps' loads in
            // flight before any is used.
            constexpr int G = 2;
            const int par = lane / 16, j = lane % 16;
            for (int b = 0; b < blocks; b += 2 * G) {
                Q4kRaw2 r[G], u[G];
                #pragma unroll
                for (int i = 0; i < G; i++) {
                    const int sb = b + 2 * i + par;
                    if (sb < blocks) {
                        r[i] = decode_q4k2_load<Prepared>(wr + sb * bytes, x + sb * QK_K, ys + sb * 8, j);
                        if constexpr (Gated) u[i] = decode_q4k2_load<Prepared>(ur + sb * bytes, x + sb * QK_K, ys + sb * 8, j);
                    }
                }
                #pragma unroll
                for (int i = 0; i < G; i++) {
                    if (b + 2 * i + par < blocks) {
                        acc += decode_q4k2_eval<Prepared>(r[i], j);
                        if constexpr (Gated) up_acc += decode_q4k2_eval<Prepared>(u[i], j);
                    }
                }
            }
        }
    }
    #pragma unroll
    for (int mask = 16; mask > 0; mask >>= 1) {
        acc += __shfl_xor(acc, mask);
        if constexpr (Gated) up_acc += __shfl_xor(up_acc, mask);
    }
    if (lane == 0 && row < n_out) {
        y[row] = Gated ? (acc / (1.0f + expf(-acc))) * up_acc : acc;
    }
}

template<bool Q6, bool Gated = false, bool Prepared = false>
__device__ __forceinline__ void decode_gemv_i8(
    const uint8_t* w, int n_out, int n_in, const int8_t* x, const float2* ys, float* y,
    const uint8_t* up = nullptr
) {
    const int row = (blockIdx.x * blockDim.x + threadIdx.x) / 32;
    decode_gemv_row<Q6, Gated, Prepared>(w, row, n_out, n_in, x, ys, y, up);
}

// Three matvecs on one quantized input in one launch -- the q, k and v
// projections. Warps [0, n0) take w0/y0, [n0, n0+n1) w1/y1, the rest
// w2/y2; the selection is per warp, so it is uniform. Each matrix has
// its own dtype (Q4_K_M keeps some attn_v tensors as Q6_K): the branch
// picks a whole row loop rather than branching inside it.
__device__ __forceinline__ void decode_gemv_i8_x3(
    const uint8_t* w0, int n0, int q6_0, float* y0, const uint8_t* w1, int n1, int q6_1, float* y1,
    const uint8_t* w2, int n2, int q6_2, float* y2, int n_in, const int8_t* x, const float2* ys
) {
    int row = (blockIdx.x * blockDim.x + threadIdx.x) / 32;
    const uint8_t* w;
    float* y;
    int n, q6;
    if (row < n0) {
        w = w0; y = y0; n = n0; q6 = q6_0;
    } else if (row < n0 + n1) {
        row -= n0; w = w1; y = y1; n = n1; q6 = q6_1;
    } else {
        row -= n0 + n1; w = w2; y = y2; n = n2; q6 = q6_2;
    }
    if (q6 == 2) {
        decode_gemv_row<false, false, true>(w, row, n, n_in, x, ys, y);
    } else if (q6 == 1) {
        decode_gemv_row<true>(w, row, n, n_in, x, ys, y);
    } else {
        decode_gemv_row<false>(w, row, n, n_in, x, ys, y);
    }
}

extern "C" __global__ void qmatmul_gemv_i8_q4k(
    const uint8_t* w, int n_out, int n_in, const int8_t* x, const float2* ys, float* y
) {
    decode_gemv_i8<false>(w, n_out, n_in, x, ys, y);
}

extern "C" __global__ void qmatmul_gemv_i8_q4k_prepared(
    const uint8_t* w, int n_out, int n_in, const int8_t* x, const float2* ys, float* y
) {
    decode_gemv_i8<false, false, true>(w, n_out, n_in, x, ys, y);
}

extern "C" __global__ void qmatmul_gemv_i8_q4k_prepared_gated(
    const uint8_t* w, int n_out, int n_in, const int8_t* x, const float2* ys, float* y, const uint8_t* up
) {
    decode_gemv_i8<false, true, true>(w, n_out, n_in, x, ys, y, up);
}

extern "C" __global__ void qmatmul_gemv_i8_q6k(
    const uint8_t* w, int n_out, int n_in, const int8_t* x, const float2* ys, float* y
) {
    decode_gemv_i8<true>(w, n_out, n_in, x, ys, y);
}

extern "C" __global__ void qmatmul_gemv_i8_q4k_gated(
    const uint8_t* w, int n_out, int n_in, const int8_t* x, const float2* ys, float* y, const uint8_t* up
) {
    decode_gemv_i8<false, true>(w, n_out, n_in, x, ys, y, up);
}

extern "C" __global__ void qmatmul_gemv_i8_q6k_gated(
    const uint8_t* w, int n_out, int n_in, const int8_t* x, const float2* ys, float* y, const uint8_t* up
) {
    decode_gemv_i8<true, true>(w, n_out, n_in, x, ys, y, up);
}

extern "C" __global__ void qmatmul_gemv_i8_x3(
    const uint8_t* w0, int n0, int q6_0, float* y0, const uint8_t* w1, int n1, int q6_1, float* y1,
    const uint8_t* w2, int n2, int q6_2, float* y2, int n_in, const int8_t* x, const float2* ys
) {
    decode_gemv_i8_x3(w0, n0, q6_0, y0, w1, n1, q6_1, y1, w2, n2, q6_2, y2, n_in, x, ys);
}

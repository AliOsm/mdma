template<bool MAX>
__device__ __forceinline__ float attn_wave_reduce(float x) {
    #pragma unroll
    for (int delta = 16; delta; delta >>= 1) {
        float y = __shfl_xor(x, delta, 32);
        x = MAX ? fmaxf(x, y) : x + y;
    }
    return x;
}

template<bool MAX>
__device__ __forceinline__ float attn_block_reduce(float x, float* partial) {
    const int lane = threadIdx.x & 31, wave = threadIdx.x >> 5;
    x = attn_wave_reduce<MAX>(x);
    if (lane == 0) partial[wave] = x;
    __syncthreads();
    x = lane < 4 ? partial[lane] : (MAX ? -INFINITY : 0.0f);
    x = attn_wave_reduce<MAX>(x);
    __syncthreads();
    return x;
}

// Four waves per query head; each wave reads one contiguous 128-wide key.
extern "C" __device__ __forceinline__ void attn_hd128_impl(
    const float* q, const _Float16* k, const _Float16* v, float* o,
    int n_q_heads, int n_kv_heads, int kv_len
) {
    extern __shared__ float scores[];
    __shared__ float partial[4];
    const int tid = threadIdx.x, lane = tid & 31, wave = tid >> 5;
    const int qh = blockIdx.x, kvh = qh / (n_q_heads / n_kv_heads);
    const int row = n_kv_heads * 128;
    float query[4];
    #pragma unroll
    for (int d = 0; d < 4; ++d) query[d] = q[qh * 128 + lane + d * 32];
    for (int t = wave; t < kv_len; t += 4) {
        float dot = 0.0f;
        #pragma unroll
        for (int d = 0; d < 4; ++d)
            dot += query[d] * (float)k[(size_t)t * row + kvh * 128 + lane + d * 32];
        dot = attn_wave_reduce<false>(dot);
        if (lane == 0) scores[t] = dot * rsqrtf(128.0f);
    }
    __syncthreads();
    float maximum = -INFINITY;
    for (int t = tid; t < kv_len; t += 128) maximum = fmaxf(maximum, scores[t]);
    maximum = attn_block_reduce<true>(maximum, partial);
    float sum = 0.0f;
    for (int t = tid; t < kv_len; t += 128) {
        float e = expf(scores[t] - maximum);
        scores[t] = e;
        sum += e;
    }
    sum = attn_block_reduce<false>(sum, partial);
    for (int t = tid; t < kv_len; t += 128) scores[t] /= sum;
    __syncthreads();
    float acc = 0.0f;
    for (int t = 0; t < kv_len; ++t)
        acc += scores[t] * (float)v[(size_t)t * row + kvh * 128 + tid];
    o[qh * 128 + tid] = acc;
}

extern "C" __global__ void attn_decode_hd128(
    const float* q, const _Float16* k, const _Float16* v, float* o,
    int, int n_q_heads, int n_kv_heads, int kv_len
) {
    attn_hd128_impl(q, k, v, o, n_q_heads, n_kv_heads, kv_len);
}

extern "C" __global__ void attn_decode_hd128_graph(
    const float* q, const _Float16* k, const _Float16* v, float* o,
    int, int n_q_heads, int n_kv_heads, int, const unsigned int* params
) {
    attn_hd128_impl(q, k, v, o, n_q_heads, n_kv_heads, params[1] + 1);
}

// One block per (KV head, ATT_DC-key chunk), one warp per query head.
// Each block writes unnormalized partials (o, m, l); the last block for
// a KV head merges chunks in fixed index order. Blocks past kv_len exit,
// allowing a graph sized for capacity to replay at shorter live lengths.
#define ATT_DC 32          // keys per block
#define ATT_DKS 136        // Ks row stride in halves: 68 dwords = 4 mod 8, conflict-free 16-byte per-lane key reads
#define ATT_DPART 132      // floats per (head, chunk) partial: o[128], m, l, pad
#define ATT_DMAX_CHUNKS 128   // 4096 keys / ATT_DC
// Compile-time GROUP gives staging fixed trip counts.
// Only K is staged in LDS, as f16; V is read from global memory in
// contiguous eight-byte spans per lane, shared through L2.
// The merge is fused: each block bumps its KV head's arrival counter
// after publishing its partial (release fence before, acquire after),
// and the last block to arrive combines every chunk for the group's
// heads in index order -- deterministic -- and resets the counter for
// the next launch.
template <int GROUP>
__device__ __forceinline__ void attn_split_hd128_body(
    const float* q, const _Float16* k, const _Float16* v, float* part, unsigned* counters, float* o, int8_t* q8, float2* ys,
    int n_kv_heads, int kv_len
) {
    constexpr int THREADS = GROUP * 32;
    constexpr int CHUNKS8 = ATT_DC * 16;       // half8s per K (or V) chunk
    static_assert(CHUNKS8 % THREADS == 0, "chunk must divide evenly over the block");
    constexpr int IT = CHUNKS8 / THREADS;
    const int kvh = blockIdx.x, chunk = blockIdx.y;
    const int t0 = chunk * ATT_DC;
    if (t0 >= kv_len) return;
    const int tid = threadIdx.x, lane = tid & 31, warp = tid >> 5;
    const int qh = kvh * GROUP + warp;
    const int row = n_kv_heads * 128;

    __shared__ __attribute__((aligned(16))) _Float16 Ks[ATT_DC * ATT_DKS];
    __shared__ __attribute__((aligned(16))) float qs[GROUP * 128];
    __shared__ float ps[GROUP][ATT_DC];
    __shared__ float wsh[GROUP][ATT_DMAX_CHUNKS];
    __shared__ int last;

    if (tid < GROUP * 32) {
        float4 qq;
        memcpy(&qq, q + (size_t)(kvh * GROUP) * 128 + tid * 4, 16);
        memcpy(&qs[tid * 4], &qq, 16);
    }
    // Keys past kv_len stage the clamped last key (finite) and are masked
    // below. All of a thread's loads are issued before its first store.
    {
        half8_t kk[IT];
        #pragma unroll
        for (int i = 0; i < IT; i++) {
            const int e = tid + i * THREADS;
            const int tt = e / 16, c8 = e % 16;
            const int t = min(t0 + tt, kv_len - 1);
            memcpy(&kk[i], k + (size_t)t * row + (size_t)kvh * 128 + c8 * 8, 16);
        }
        #pragma unroll
        for (int i = 0; i < IT; i++) {
            const int e = tid + i * THREADS;
            const int tt = e / 16, c8 = e % 16;
            memcpy(&Ks[tt * ATT_DKS + c8 * 8], &kk[i], 16);
        }
    }
    __syncthreads();

    float s = 0.0f;
    #pragma unroll 4
    for (int d = 0; d < 128; d += 8) {
        half8_t kh;
        float4 q0, q1;
        memcpy(&kh, &Ks[lane * ATT_DKS + d], 16);
        memcpy(&q0, &qs[warp * 128 + d], 16);
        memcpy(&q1, &qs[warp * 128 + d + 4], 16);
        s = fmaf((float)kh[0], q0.x, s);
        s = fmaf((float)kh[1], q0.y, s);
        s = fmaf((float)kh[2], q0.z, s);
        s = fmaf((float)kh[3], q0.w, s);
        s = fmaf((float)kh[4], q1.x, s);
        s = fmaf((float)kh[5], q1.y, s);
        s = fmaf((float)kh[6], q1.z, s);
        s = fmaf((float)kh[7], q1.w, s);
    }
    const bool live = t0 + lane < kv_len;
    s = live ? s * rsqrtf(128.0f) : -INFINITY;
    const float m = attn_wave_reduce<true>(s);      // finite: lane 0's key is live
    const float p = live ? expf(s - m) : 0.0f;
    const float l = attn_wave_reduce<false>(p);
    ps[warp][lane] = p;
    __syncthreads();

    float4 po = make_float4(0.0f, 0.0f, 0.0f, 0.0f);
    const _Float16* vbase = v + (size_t)kvh * 128 + lane * 4;
    #pragma unroll 8
    for (int tt = 0; tt < ATT_DC; tt++) {
        const float pt = ps[warp][tt];
        half4_t vv;
        memcpy(&vv, vbase + (size_t)min(t0 + tt, kv_len - 1) * row, 8);
        po.x = fmaf(pt, (float)vv[0], po.x);
        po.y = fmaf(pt, (float)vv[1], po.y);
        po.z = fmaf(pt, (float)vv[2], po.z);
        po.w = fmaf(pt, (float)vv[3], po.w);
    }
    float* dst = part + ((size_t)qh * gridDim.y + chunk) * ATT_DPART;
    memcpy(dst + lane * 4, &po, 16);
    if (lane == 0) {
        dst[128] = m;
        dst[129] = l;
    }

    const int n_live = (kv_len + ATT_DC - 1) / ATT_DC;
    __threadfence();
    __syncthreads();
    if (tid == 0) last = atomicAdd(&counters[kvh], 1u) == (unsigned)(n_live - 1);
    __syncthreads();
    if (!last) return;
    __threadfence();

    // Chunk statistics for this warp's head: max over chunks, weights to
    // LDS, weighted denominator -- then each lane's four dims over the
    // chunks as independent loads.
    const float* base = part + (size_t)qh * gridDim.y * ATT_DPART;
    float mc[ATT_DMAX_CHUNKS / 32];
    float mx = -INFINITY;
    #pragma unroll
    for (int i = 0; i < ATT_DMAX_CHUNKS / 32; i++) {
        const int c = lane + i * 32;
        mc[i] = c < n_live ? base[c * ATT_DPART + 128] : -INFINITY;
        mx = fmaxf(mx, mc[i]);
    }
    mx = attn_wave_reduce<true>(mx);
    float lsum = 0.0f;
    #pragma unroll
    for (int i = 0; i < ATT_DMAX_CHUNKS / 32; i++) {
        const int c = lane + i * 32;
        const float w = c < n_live ? expf(mc[i] - mx) : 0.0f;
        wsh[warp][c] = w;
        lsum += c < n_live ? base[c * ATT_DPART + 129] * w : 0.0f;
    }
    lsum = attn_wave_reduce<false>(lsum);
    __syncthreads();   // every lane reads the whole wsh row its warp just wrote
    float4 acc = make_float4(0.0f, 0.0f, 0.0f, 0.0f);
    #pragma unroll 8
    for (int c = 0; c < n_live; c++) {
        const float w = wsh[warp][c];
        float4 oc;
        memcpy(&oc, base + c * ATT_DPART + lane * 4, 16);
        acc.x = fmaf(w, oc.x, acc.x);
        acc.y = fmaf(w, oc.y, acc.y);
        acc.z = fmaf(w, oc.z, acc.z);
        acc.w = fmaf(w, oc.w, acc.w);
    }
    const float inv = 1.0f / lsum;
    const float4 res = make_float4(acc.x * inv, acc.y * inv, acc.z * inv, acc.w * inv);
    memcpy(o + (size_t)qh * 128 + lane * 4, &res, 16);
    // The output projection consumes this row as Q8_1: emit it here so
    // that matvec skips its quantize launch.
    q8_emit_chunk(res, qh * 32 + lane, q8, ys);
    if (tid == 0) counters[kvh] = 0u;
}

extern "C" __global__ void __launch_bounds__(128) attn_decode_split_hd128_g4(
    const float* q, const _Float16* k, const _Float16* v, float* part, unsigned* counters, float* o, int8_t* q8, float2* ys,
    int n_kv_heads, int kv_len
) {
    attn_split_hd128_body<4>(q, k, v, part, counters, o, q8, ys, n_kv_heads, kv_len);
}

extern "C" __global__ void __launch_bounds__(128) attn_decode_split_hd128_g4_graph(
    const float* q, const _Float16* k, const _Float16* v, float* part, unsigned* counters, float* o, int8_t* q8, float2* ys,
    int n_kv_heads, int, const unsigned int* params
) {
    attn_split_hd128_body<4>(q, k, v, part, counters, o, q8, ys, n_kv_heads, params[1] + 1);
}

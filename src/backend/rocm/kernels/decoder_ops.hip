extern "C" __global__ void unary_kernel(float* x, int n, float value, int op) {
    int i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i < n) x[i] = op == 0 ? x[i] * value : tanhf(x[i] / value) * value;
}

extern "C" __global__ void add_bias_kernel(const float* b, float* y, int width, int n) {
    int i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i < n) y[i] += b[i % width];
}

extern "C" __global__ void gelu_tanh_mul_kernel(const float* a, const float* b, float* y, int n) {
    int i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i < n) {
        float x = a[i];
        y[i] = 0.5f * x * (1.0f + tanhf(0.7978846f * x * (1.0f + 0.044715f * x * x))) * b[i];
    }
}

extern "C" __global__ void rope_split_half_kernel(
    float* x, const float* factors, int n, int hd, unsigned int pos, float theta
) {
    int pair = blockIdx.x * blockDim.x + threadIdx.x;
    if (pair >= n / 2) return;
    int half = hd / 2, j = pair % half, i = (pair / half) * hd + j;
    float angle = (float)pos * powf(theta, -2.0f * j / hd) / factors[j];
    float s, c;
    sincosf(angle, &s, &c);
    float a = x[i], b = x[i + half];
    x[i] = a * c - b * s;
    x[i + half] = a * s + b * c;
}

__device__ __forceinline__ float decoder_kv(const void* p, size_t i, int f32_kv) {
    return f32_kv ? ((const float*)p)[i] : (float)((const _Float16*)p)[i];
}

extern "C" __global__ void rope_partial_interleaved_kernel(
    float* x, int pairs, int hd, int rot, unsigned int pos, float theta
) {
    int pair = blockIdx.x * blockDim.x + threadIdx.x;
    if (pair >= pairs) return;
    int j = pair % (rot / 2);
    size_t i = (size_t)(pair / (rot / 2)) * hd + 2 * j;
    float angle = (float)pos * powf(theta, -2.0f * j / rot);
    float s, c;
    sincosf(angle, &s, &c);
    float a = x[i], b = x[i + 1];
    x[i] = a * c - b * s;
    x[i + 1] = a * s + b * c;
}

// One 128-thread block per query/head. Fixed LDS, even for long global contexts.
extern "C" __global__ void attn_windowed_kernel(
    const float* q, const void* k, const void* v, float* o,
    int hd, int qheads, int kvheads, int prefix, float scale, int window, int f32_kv
) {
    constexpr int CHUNK = 256;
    __shared__ float qs[512], sc[CHUNK], red[128];
    int lid = threadIdx.x, qh = blockIdx.x, qi = blockIdx.y;
    int kvh = qh / (qheads / kvheads);
    size_t qbase = ((size_t)qi * qheads + qh) * hd;
    size_t kvrow = (size_t)kvheads * hd;
    int visible = prefix + qi + 1;
    int start = window == 0 ? 0 : max(0, visible - window);
    for (int d = lid; d < hd; d += 128) qs[d] = q[qbase + d];
    __syncthreads();
    float m = -INFINITY, sum = 0.0f, acc[4] = {};
    for (int c0 = start; c0 < visible; c0 += CHUNK) {
        int n = min(CHUNK, visible - c0);
        float local_max = -INFINITY;
        for (int t = lid; t < n; t += 128) {
            size_t base = (size_t)(c0 + t) * kvrow + kvh * hd;
            float dot = 0.0f;
            for (int d = 0; d < hd; d++) dot += qs[d] * decoder_kv(k, base + d, f32_kv);
            sc[t] = dot * scale;
            local_max = fmaxf(local_max, sc[t]);
        }
        red[lid] = local_max;
        __syncthreads();
        for (int s = 64; s > 0; s >>= 1) {
            if (lid < s) red[lid] = fmaxf(red[lid], red[lid + s]);
            __syncthreads();
        }
        float next = fmaxf(m, red[0]), corr = expf(m - next);
        __syncthreads();
        sum *= corr;
        for (int j = 0; j < 4; j++) acc[j] *= corr;
        float local_sum = 0.0f;
        for (int t = lid; t < n; t += 128) {
            sc[t] = expf(sc[t] - next);
            local_sum += sc[t];
        }
        red[lid] = local_sum;
        __syncthreads();
        for (int s = 64; s > 0; s >>= 1) {
            if (lid < s) red[lid] += red[lid + s];
            __syncthreads();
        }
        sum += red[0];
        for (int t = 0; t < n; t++) {
            size_t base = (size_t)(c0 + t) * kvrow + kvh * hd;
            for (int j = 0; j < 4; j++) {
                int d = lid + 128 * j;
                if (d < hd) acc[j] += sc[t] * decoder_kv(v, base + d, f32_kv);
            }
        }
        __syncthreads();
        m = next;
    }
    for (int j = 0; j < 4; j++) {
        int d = lid + 128 * j;
        if (d < hd) o[qbase + d] = acc[j] / sum;
    }
}

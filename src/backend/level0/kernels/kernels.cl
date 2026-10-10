// Level Zero kernels: f32 activations with inline F16/Q4_K/Q6_K dequantization.
// One work-item per output element except matvec, which is one subgroup
// per output row.

__kernel void copy_f32(__global const float* x, __global float* y, int n) {
    int i = get_global_id(0);
    if (i < n) y[i] = x[i];
}

__kernel void add_f32(__global const float* a, __global const float* b, __global float* y, int n) {
    int i = get_global_id(0);
    if (i < n) y[i] = a[i] + b[i];
}

__kernel void rmsnorm_f32(__global const float* w, __global const float* x, __global float* y, int dim, float eps) {
    int t = get_global_id(0);
    float ss = 0.0f;
    for (int i = 0; i < dim; i++) {
        float v = x[t * dim + i];
        ss += v * v;
    }
    float scale = 1.0f / sqrt(ss / (float)dim + eps);
    for (int i = 0; i < dim; i++) y[t * dim + i] = x[t * dim + i] * scale * w[i];
}

__kernel void silu_mul_f32(__global const float* gate, __global const float* up, __global float* y, int n) {
    int i = get_global_id(0);
    if (i < n) {
        float g = gate[i];
        y[i] = (g / (1.0f + exp(-g))) * up[i];
    }
}

// buf is [n_tokens, n_heads, head_dim], interleaved pairs (2i, 2i+1).
__kernel void rope_f32(__global float* q, __global float* k, int pos, int head_dim, int n_q_heads, int n_kv_heads, float theta) {
    int pair = get_global_id(0);
    int head = get_global_id(1);
    int t = get_global_id(2);
    int n_pairs = head_dim / 2;
    int n_heads = (head < n_q_heads) ? n_q_heads : n_kv_heads;
    int local_head = (head < n_q_heads) ? head : head - n_q_heads;
    if (pair >= n_pairs || local_head >= n_heads) return;
    __global float* buf = (head < n_q_heads) ? q : k;
    int stride = n_heads * head_dim;
    float freq = pow(theta, -2.0f * (float)pair / (float)head_dim);
    float ang = (float)(pos + t) * freq;
    float s, c;
    s = sincos(ang, &c);
    int base = t * stride + local_head * head_dim + 2 * pair;
    float x0 = buf[base];
    float x1 = buf[base + 1];
    buf[base] = x0 * c - x1 * s;
    buf[base + 1] = x0 * s + x1 * c;
}

// q/o: [n_q, n_q_heads, head_dim]. kv rows: [kv_len, n_kv_heads, head_dim].
// Query i attends to keys [0, prefix + i]. One work-item per query head.
__kernel void attn_f32(
    __global const float* q,
    __global const float* k,
    __global const float* v,
    __global float* o,
    int head_dim,
    int n_q_heads,
    int n_kv_heads,
    int n_q,
    int prefix
) {
    int qh = get_global_id(0);
    int qi = get_global_id(1);
    if (qh >= n_q_heads || qi >= n_q) return;
    int group = n_q_heads / n_kv_heads;
    int kvh = qh / group;
    int kv_row = n_kv_heads * head_dim;
    float scale = 1.0f / sqrt((float)head_dim);
    int q_base = (qi * n_q_heads + qh) * head_dim;
    int visible = prefix + qi + 1;

    float max_s = -INFINITY;
    for (int t = 0; t < visible; t++) {
        int k_base = t * kv_row + kvh * head_dim;
        float dot = 0.0f;
        for (int d = 0; d < head_dim; d++) dot += q[q_base + d] * k[k_base + d];
        max_s = fmax(max_s, dot * scale);
    }
    float sum = 0.0f;
    for (int t = 0; t < visible; t++) {
        int k_base = t * kv_row + kvh * head_dim;
        float dot = 0.0f;
        for (int d = 0; d < head_dim; d++) dot += q[q_base + d] * k[k_base + d];
        sum += exp(dot * scale - max_s);
    }
    for (int d = 0; d < head_dim; d++) o[q_base + d] = 0.0f;
    for (int t = 0; t < visible; t++) {
        int k_base = t * kv_row + kvh * head_dim;
        float dot = 0.0f;
        for (int d = 0; d < head_dim; d++) dot += q[q_base + d] * k[k_base + d];
        float w = exp(dot * scale - max_s) / sum;
        int v_base = t * kv_row + kvh * head_dim;
        for (int d = 0; d < head_dim; d++) o[q_base + d] += w * v[v_base + d];
    }
}

// dtype: 0 f32, 1 f16. y[t, row] = dot(W[row], x[t]).
__kernel void gemv_f(
    __global const uchar* w,
    __global const float* x,
    __global float* y,
    int n_out,
    int n_in,
    int n_tokens,
    int dtype
) {
    int row = get_global_id(0);
    int t = get_global_id(1);
    if (row >= n_out || t >= n_tokens) return;
    float acc = 0.0f;
    if (dtype == 0) {
        __global const float* wr = ((__global const float*)w) + (size_t)row * n_in;
        for (int i = 0; i < n_in; i++) acc += wr[i] * x[t * n_in + i];
    } else {
        __global const half* wr = ((__global const half*)w) + (size_t)row * n_in;
        for (int i = 0; i < n_in; i++) acc += (float)wr[i] * x[t * n_in + i];
    }
    y[t * n_out + row] = acc;
}

// Subgroup matvec: 16 lanes share one output row. n_out is the real row
// count; the host launches a multiple of the subgroup so partial rows mask.
__kernel void gemv_f_sg(
    __global const uchar* w,
    __global const float* x,
    __global float* y,
    int n_out,
    int n_in,
    int dtype
) {
    int lane = get_sub_group_local_id();
    int row = get_group_id(0) * get_sub_group_size() + lane;
    float acc = 0.0f;
    if (row < n_out) {
        if (dtype == 0) {
            __global const float* wr = ((__global const float*)w) + (size_t)row * n_in;
            for (int i = 0; i < n_in; i++) acc += wr[i] * x[i];
        } else {
            __global const half* wr = ((__global const half*)w) + (size_t)row * n_in;
            for (int i = 0; i < n_in; i++) acc += (float)wr[i] * x[i];
        }
        y[row] = acc;
    }
}

// Q4_K block: f16 d, f16 dmin, 12 scale bytes, 128 qs bytes. 144 bytes.
// j walks eight sub-blocks of 32.
float q4k_dot(__global const uchar* blk, __global const float* x) {
    float d = (float)(*(__global const half*)blk);
    float dmin = (float)(*(__global const half*)(blk + 2));
    __global const uchar* sc = blk + 4;
    __global const uchar* qs = blk + 16;
    float acc = 0.0f;
    for (int j = 0; j < 8; j++) {
        int is = j;
        uchar scale, mn;
        if (is < 4) {
            scale = sc[is] & 63;
            mn = sc[is + 4] & 63;
        } else {
            scale = (sc[is + 4] & 0xF) | ((sc[is - 4] >> 6) << 4);
            mn = (sc[is + 4] >> 4) | ((sc[is] >> 6) << 4);
        }
        float dd = d * (float)scale;
        float mm = dmin * (float)mn;
        int qbase = (j / 2) * 32;
        int shift = (j & 1) ? 4 : 0;
        int xbase = j * 32;
        for (int l = 0; l < 32; l++) {
            float q = (float)((qs[qbase + l] >> shift) & 0xF);
            acc += (dd * q - mm) * x[xbase + l];
        }
    }
    return acc;
}

// Q6_K block: 128 ql, 64 qh, 16 i8 scales, f16 d. 210 bytes.
float q6k_dot(__global const uchar* blk, __global const float* x) {
    __global const uchar* ql = blk;
    __global const uchar* qh = blk + 128;
    __global const char* scales = (__global const char*)(blk + 192);
    float d = (float)(*(__global const half*)(blk + 208));
    float acc = 0.0f;
    for (int jb = 0; jb < 2; jb++) {
        int j = jb * 128;
        for (int l = 0; l < 32; l++) {
            int q0 = (int)(ql[j / 2 + l] & 0xF) | ((int)(qh[j / 4 + l] & 3) << 4);
            int q1 = (int)(ql[j / 2 + l + 32] & 0xF) | (((int)(qh[j / 4 + l] >> 2) & 3) << 4);
            int q2 = (int)(ql[j / 2 + l] >> 4) | (((int)(qh[j / 4 + l] >> 4) & 3) << 4);
            int q3 = (int)(ql[j / 2 + l + 32] >> 4) | (((int)(qh[j / 4 + l] >> 6) & 3) << 4);
            int s0 = (int)scales[(j + l) / 16];
            int s1 = (int)scales[(j + l + 32) / 16];
            int s2 = (int)scales[(j + l + 64) / 16];
            int s3 = (int)scales[(j + l + 96) / 16];
            acc += d * (float)s0 * (float)(q0 - 32) * x[j + l];
            acc += d * (float)s1 * (float)(q1 - 32) * x[j + l + 32];
            acc += d * (float)s2 * (float)(q2 - 32) * x[j + l + 64];
            acc += d * (float)s3 * (float)(q3 - 32) * x[j + l + 96];
        }
    }
    return acc;
}

// dtype 2 = Q4_K (144 B / 256), 3 = Q6_K (210 B / 256). n_in % 256 == 0.
__kernel void gemv_q(
    __global const uchar* w,
    __global const float* x,
    __global float* y,
    int n_out,
    int n_in,
    int n_tokens,
    int dtype,
    int block_bytes
) {
    int row = get_global_id(0);
    int t = get_global_id(1);
    if (row >= n_out || t >= n_tokens) return;
    int n_blocks = n_in / 256;
    __global const uchar* rowp = w + (size_t)row * n_blocks * block_bytes;
    __global const float* xt = x + t * n_in;
    float acc = 0.0f;
    for (int b = 0; b < n_blocks; b++) {
        if (dtype == 2) acc += q4k_dot(rowp + b * block_bytes, xt + b * 256);
        else acc += q6k_dot(rowp + b * block_bytes, xt + b * 256);
    }
    y[t * n_out + row] = acc;
}

// Gather dequantized rows. Same dtype codes as gemv_q, plus 0/1 for f32/f16.
__kernel void embed_rows(
    __global const uchar* table,
    __global const int* ids,
    __global float* out,
    int n_embd,
    int n_tokens,
    int dtype,
    int block_bytes
) {
    int t = get_global_id(0);
    int i = get_global_id(1);
    if (t >= n_tokens || i >= n_embd) return;
    int id = ids[t];
    float v = 0.0f;
    switch (dtype) {
    case 0:
        v = ((__global const float*)table)[(size_t)id * n_embd + i];
        break;
    case 1:
        v = (float)((__global const half*)table)[(size_t)id * n_embd + i];
        break;
    default: {
        int n_blocks = n_embd / 256;
        __global const uchar* rowp = table + (size_t)id * n_blocks * block_bytes;
        int b = i / 256;
        int e = i % 256;
        // Reconstruct one element by the same formula as the dot kernels.
        if (dtype == 2) {
            __global const uchar* blk = rowp + b * block_bytes;
            float d = (float)(*(__global const half*)blk);
            float dmin = (float)(*(__global const half*)(blk + 2));
            __global const uchar* sc = blk + 4;
            __global const uchar* qs = blk + 16;
            int j = e / 32;
            int l = e % 32;
            uchar scale, mn;
            if (j < 4) { scale = sc[j] & 63; mn = sc[j + 4] & 63; }
            else {
                scale = (sc[j + 4] & 0xF) | ((sc[j - 4] >> 6) << 4);
                mn = (sc[j + 4] >> 4) | ((sc[j] >> 6) << 4);
            }
            int qbase = (j / 2) * 32;
            int shift = (j & 1) ? 4 : 0;
            float q = (float)((qs[qbase + l] >> shift) & 0xF);
            v = d * (float)scale * q - dmin * (float)mn;
        } else {
            __global const uchar* blk = rowp + b * block_bytes;
            __global const uchar* ql = blk;
            __global const uchar* qh = blk + 128;
            __global const char* scales = (__global const char*)(blk + 192);
            float d = (float)(*(__global const half*)(blk + 208));
            int jb = e / 128;
            int within = e % 128;
            int j = jb * 128;
            int quad = within / 32;
            int l = within % 32;
            int qv;
            switch (quad) {
            case 0: qv = (int)(ql[j / 2 + l] & 0xF) | ((int)(qh[j / 4 + l] & 3) << 4); break;
            case 1: qv = (int)(ql[j / 2 + l + 32] & 0xF) | (((int)(qh[j / 4 + l] >> 2) & 3) << 4); break;
            case 2: qv = (int)(ql[j / 2 + l] >> 4) | (((int)(qh[j / 4 + l] >> 4) & 3) << 4); break;
            default: qv = (int)(ql[j / 2 + l + 32] >> 4) | (((int)(qh[j / 4 + l] >> 6) & 3) << 4); break;
            }
            int sc = (int)scales[(j + quad * 32 + l) / 16];
            v = d * (float)sc * (float)(qv - 32);
        }
        break;
    }
    }
    out[t * n_embd + i] = v;
}

// sum = a + b, then RMSNorm(sum) into y. One launch instead of add + rmsnorm.
__kernel void add_rmsnorm_f32(
    __global const float* w,
    __global const float* a,
    __global const float* b,
    __global float* sum,
    __global float* y,
    int dim,
    float eps
) {
    int t = get_global_id(0);
    float ss = 0.0f;
    for (int i = 0; i < dim; i++) {
        float v = a[t * dim + i] + b[t * dim + i];
        sum[t * dim + i] = v;
        ss += v * v;
    }
    float scale = 1.0f / sqrt(ss / (float)dim + eps);
    for (int i = 0; i < dim; i++) y[t * dim + i] = sum[t * dim + i] * scale * w[i];
}

// Output-stationary LDS-tiled K-quant GEMM for prefill. A_F16 stages
// weights and activations as f16; products and sums are always f32.
//
// A 256-lane workgroup owns a 128 x 64 output tile. Two lanes share
// each weight row; four share each token. Each lane computes an 8 x 4
// microtile. The next K step is fetched into registers before this
// step's compute and staged in LDS afterward.
#extension GL_GOOGLE_include_directive : require
#extension GL_EXT_shader_explicit_arithmetic_types_int8 : require
#ifdef A_F16
#extension GL_EXT_shader_explicit_arithmetic_types_float16 : require
#endif
#include "common.glsl"
#include "kquant.glsl"
layout(local_size_x = 256) in;
layout(push_constant, std430) uniform PC { uint64_t w; uint64_t x; uint64_t y; uint n_out; uint n_in; uint n_tokens; uint dtype; uint block_bytes; } pc;
layout(buffer_reference, std430, buffer_reference_align = 16) buffer Vec4Buf { vec4 v[]; };
#ifndef BK_STEP
#define BK_STEP 32u
#endif
const uint BM = 128u, BN = 64u, BK = BK_STEP;
// Elements a lane stages per step: BK/2 of a row (QA quads), BK/4 of a token (QB quads).
const uint QA = BK / 8u, QB = BK / 16u;
#ifdef A_F16
shared f16vec4 As[BK][BM / 4u];
shared f16vec4 Bs[BK][BN / 4u];
#else
shared vec4 As[BK][BM / 4u];
shared vec4 Bs[BK][BN / 4u];
#endif

// Per-superblock header words, fetched when a step enters a new block.
// Q4_K: [0] d|dmin, [1..3] the 12 scale bytes. Q6_K: [0] the d's word.
// Per-step words: Q4_K: the QA nibble words. Q6_K (blocks are only
// 2-byte aligned, so a lane's 16 consecutive bytes of ql or qh span 5
// words): [0..4] ql words, [5..9] qh words, [10] the scale's word.
const uint RAW = 2u * QA + 3u;

void fetch_hdr(U32Buf w, bool q4, uint blk, out uint hdr[4]) {
    if (q4) {
        uint w0 = blk / 4u;
        hdr[0] = w.v[w0];
        hdr[1] = w.v[w0 + 1u];
        hdr[2] = w.v[w0 + 2u];
        hdr[3] = w.v[w0 + 3u];
    } else {
        hdr[0] = w.v[(blk + 208u) >> 2u];
        hdr[1] = 0u;
        hdr[2] = 0u;
        hdr[3] = 0u;
    }
}

void fetch(U32Buf w, bool q4, uint blk, uint e0, out uint raw[RAW]) {
    if (q4) {
        Q4Lane L = q4_lane_at(e0);
        uint w0 = blk / 4u;
        for (uint q = 0u; q < QA; q++) raw[q] = w.v[w0 + L.qw + q];
        for (uint i = QA; i < RAW; i++) raw[i] = 0u;
    } else {
        // The lane's 16 elements are consecutive within one ql run and
        // one qh run (e0 % 16 == 0); all words lie inside the block.
        Q6Lane L = q6_lane_at(e0);
        uint a = (blk + L.ql_off) >> 2u, b = (blk + L.qh_off) >> 2u, c = (blk + L.sc_off) >> 2u;
        for (uint i = 0u; i <= QA; i++) raw[i] = w.v[a + i];
        for (uint i = 0u; i <= QA; i++) raw[QA + 1u + i] = w.v[b + i];
        raw[2u * QA + 2u] = w.v[c];
    }
}

// The word at bit shift `sh` (0 or 16) into the pair (lo, hi).
uint combine(uint lo, uint hi, uint sh) {
    return sh == 0u ? lo : (lo >> sh) | (hi << (32u - sh));
}

void stage_a(uint k, uint srow, vec4 q) {
#ifdef A_F16
    f16vec4 h = f16vec4(q);
    As[k][srow / 4u][srow % 4u] = h.x;
    As[k + 1u][srow / 4u][srow % 4u] = h.y;
    As[k + 2u][srow / 4u][srow % 4u] = h.z;
    As[k + 3u][srow / 4u][srow % 4u] = h.w;
#else
    As[k][srow / 4u][srow % 4u] = q.x;
    As[k + 1u][srow / 4u][srow % 4u] = q.y;
    As[k + 2u][srow / 4u][srow % 4u] = q.z;
    As[k + 3u][srow / 4u][srow % 4u] = q.w;
#endif
}

// Dequantize the lane's 16 elements (block byte offset blk, element e0
// within it) into As at k offset kk.
void stage(bool q4, uint hdr[4], uint raw[RAW], uint blk, uint e0, uint srow, uint kk) {
    if (q4) {
        Q4Lane L = q4_lane_at(e0);
        vec2 dm = unpackHalf2x16(hdr[0]);
        uint scale, mn;
        if (L.j < 4u) {
            scale = byte_of(hdr[1], L.j) & 63u;
            mn = byte_of(hdr[2], L.j) & 63u;
        } else {
            uint k = L.j - 4u;
            scale = (byte_of(hdr[3], k) & 0xFu) | ((byte_of(hdr[1], k) >> 6u) << 4u);
            mn = (byte_of(hdr[3], k) >> 4u) | ((byte_of(hdr[2], k) >> 6u) << 4u);
        }
        float d = dm.x * float(scale), m = dm.y * float(mn);
        for (uint q = 0u; q < QA; q++) {
            uint q4w = raw[q] >> L.shift;
            vec4 qv = vec4(unpack8(q4w & 0x0F0F0F0Fu));
            stage_a(kk + 4u * q, srow, fma(vec4(d), qv, vec4(-m)));
        }
    } else {
        uint dw = blk + 208u;
        vec2 dh = unpackHalf2x16(hdr[0]);
        float d = (dw & 2u) == 0u ? dh.x : dh.y;
        Q6Lane L = q6_lane_at(e0);
        uint sh = ((blk + L.ql_off) & 3u) * 8u;
        float s = float(int(byte_of(raw[2u * QA + 2u], (blk + L.sc_off) & 3u) << 24) >> 24);
        for (uint q = 0u; q < QA; q++) {
            uint ql4 = combine(raw[q], raw[q + 1u], sh) >> L.ql_shift;
            uint qh4 = combine(raw[QA + 1u + q], raw[QA + 2u + q], sh) >> L.qh_shift;
            uint qq = (ql4 & 0x0F0F0F0Fu) | ((qh4 & 0x03030303u) << 4u);
            vec4 qv = vec4(unpack8(qq)) - vec4(32.0);
            stage_a(kk + 4u * q, srow, (d * s) * qv);
        }
    }
}

void stage_b(vec4 xr[QB], uint stok, uint kk) {
    for (uint q = 0u; q < QB; q++) {
#ifdef A_F16
        f16vec4 v = f16vec4(xr[q]);
#else
        vec4 v = xr[q];
#endif
        Bs[kk + 4u * q][stok / 4u][stok % 4u] = v.x;
        Bs[kk + 4u * q + 1u][stok / 4u][stok % 4u] = v.y;
        Bs[kk + 4u * q + 2u][stok / 4u][stok % 4u] = v.z;
        Bs[kk + 4u * q + 3u][stok / 4u][stok % 4u] = v.w;
    }
}

void main() {
    uint row0 = gl_WorkGroupID.x * BM;
    uint t0 = gl_WorkGroupID.y * BN;
    uint lane = gl_LocalInvocationID.x;
    U32Buf w = U32Buf(pc.w);
    Vec4Buf x = Vec4Buf(pc.x);
    uint n_blocks = pc.n_in / 256u;
    uint xstride = pc.n_in / 4u;
    bool q4 = pc.dtype == DTYPE_Q4K;
    // Staging roles (rows/tokens past the end: clamped for the loads,
    // never written). Row srow, its k half ka; token stok, its k quarter kb.
    uint srow = lane % 128u, ka = (lane / 128u) * (BK / 2u);
    uint stok = lane % 64u, kb = (lane / 64u) * (BK / 4u);
    uint rowp = min(row0 + srow, pc.n_out - 1u) * n_blocks * pc.block_bytes;
    uint xb = min(t0 + stok, pc.n_tokens - 1u) * xstride + kb / 4u;
    // Compute: row quads ra, ra + 16 (rows 4ra.. and 64 + 4ra..), token quad cb.
    uint ra = lane % 16u, cb = lane / 16u;
    vec4 acc[2][4];
    for (uint i = 0u; i < 2u; i++)
        for (uint j = 0u; j < 4u; j++) acc[i][j] = vec4(0.0);

    uint hdr[4];
    uint raw[RAW];
    vec4 xr[QB];
    fetch_hdr(w, q4, rowp, hdr);
    fetch(w, q4, rowp, ka, raw);
    for (uint q = 0u; q < QB; q++) xr[q] = x.v[xb + q];
    stage(q4, hdr, raw, rowp, ka, srow, ka);
    stage_b(xr, stok, kb);
    for (uint k0 = 0u; k0 < pc.n_in; k0 += BK) {
        uint k1 = k0 + BK;
        bool more = k1 < pc.n_in;
        uint blk1 = rowp + (k1 / 256u) * pc.block_bytes;
        uint e1 = k1 % 256u + ka;
        if (more) {
            if (k1 % 256u == 0u) fetch_hdr(w, q4, blk1, hdr);
            fetch(w, q4, blk1, e1, raw);
            for (uint q = 0u; q < QB; q++) xr[q] = x.v[xb + k1 / 4u + q];
        }
        barrier();
        for (uint k = 0u; k < BK; k++) {
            vec4 a0 = vec4(As[k][ra]);
            vec4 a1 = vec4(As[k][ra + 16u]);
            vec4 bv = vec4(Bs[k][cb]);
            acc[0][0] = fma(a0, vec4(bv.x), acc[0][0]);
            acc[0][1] = fma(a0, vec4(bv.y), acc[0][1]);
            acc[0][2] = fma(a0, vec4(bv.z), acc[0][2]);
            acc[0][3] = fma(a0, vec4(bv.w), acc[0][3]);
            acc[1][0] = fma(a1, vec4(bv.x), acc[1][0]);
            acc[1][1] = fma(a1, vec4(bv.y), acc[1][1]);
            acc[1][2] = fma(a1, vec4(bv.z), acc[1][2]);
            acc[1][3] = fma(a1, vec4(bv.w), acc[1][3]);
        }
        barrier();
        if (more) {
            stage(q4, hdr, raw, blk1, e1, srow, ka);
            stage_b(xr, stok, kb);
        }
    }
    F32Buf y = F32Buf(pc.y);
    for (uint j = 0u; j < 4u; j++) {
        uint t = t0 + cb * 4u + j;
        if (t >= pc.n_tokens) continue;
        for (uint i = 0u; i < 8u; i++) {
            uint r = row0 + (ra + (i / 4u) * 16u) * 4u + i % 4u;
            if (r < pc.n_out) y.v[t * pc.n_out + r] = acc[i / 4u][j][i % 4u];
        }
    }
}

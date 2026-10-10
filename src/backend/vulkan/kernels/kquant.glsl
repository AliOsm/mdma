// K-quant superblock dequant for one lane's four consecutive elements,
// shared by the matvec (gemv_q) and the batched matmul (gemm_q).
// dtype 2 = Q4_K (144 B / 256), 3 = Q6_K (210 B / 256).
// Lane l owns elements 4l..4l+3 of every superblock;
// the sub-block indices that implies are the same in every superblock,
// so callers compute them once (Q4Lane / Q6Lane) outside the K loop.
//
// A dequantized quad is `d * q - m` per element: the dot with x is
// d * dot(q, x) - m * sum(x).

// Four consecutive bytes at any byte offset (two word loads when unaligned).
uint bytes4_at(U32Buf b, uint i) {
    uint lo = b.v[i >> 2];
    uint sh = (i & 3u) * 8u;
    if (sh == 0u) return lo;
    uint hi = b.v[(i >> 2) + 1u];
    return (lo >> sh) | (hi << (32u - sh));
}

uint byte_of(uint word, uint i) {
    return (word >> (i * 8u)) & 0xFFu;
}

struct Quad {
    vec4 q;
    float d;
    float m;
};

// Q4_K: `j` the lane's 32-wide sub-block, `qw` the word index of its
// four nibbles within the block, `shift` 0 or 4. Blocks are 16-byte
// aligned, so words are read directly and `w0` is the block's first word.
struct Q4Lane { uint j; uint qw; uint shift; };

// For the quad starting at element e0 (a multiple of 4) of a superblock.
Q4Lane q4_lane_at(uint e0) {
    uint j = e0 / 32u, l = e0 % 32u;
    Q4Lane r;
    r.j = j;
    r.qw = 4u + (j / 2u) * 8u + l / 4u;
    r.shift = (j & 1u) != 0u ? 4u : 0u;
    return r;
}

Q4Lane q4_lane(uint lane) {
    return q4_lane_at(lane * 4u);
}

Quad q4k_quad(U32Buf w, uint w0, Q4Lane L) {
    vec2 dm = unpackHalf2x16(w.v[w0]);
    uint s0 = w.v[w0 + 1u], s1 = w.v[w0 + 2u], s2 = w.v[w0 + 3u];
    uint scale, mn;
    if (L.j < 4u) {
        scale = byte_of(s0, L.j) & 63u;
        mn = byte_of(s1, L.j) & 63u;
    } else {
        uint k = L.j - 4u;
        scale = (byte_of(s2, k) & 0xFu) | ((byte_of(s0, k) >> 6u) << 4u);
        mn = (byte_of(s2, k) >> 4u) | ((byte_of(s1, k) >> 6u) << 4u);
    }
    uint q4 = w.v[w0 + L.qw] >> L.shift;
    Quad r;
    r.q = vec4(float(q4 & 0xFu), float((q4 >> 8u) & 0xFu), float((q4 >> 16u) & 0xFu), float((q4 >> 24u) & 0xFu));
    r.d = dm.x * float(scale);
    r.m = dm.y * float(mn);
    return r;
}

// Q6_K: blocks are only 2-byte aligned; `blk` is the block's byte offset
// and the lane's offsets within a block are precomputed.
struct Q6Lane { uint ql_off; uint qh_off; uint sc_off; uint ql_shift; uint qh_shift; };

Q6Lane q6_lane_at(uint e0) {
    uint jb = e0 / 128u, within = e0 % 128u, quad = within / 32u, l = within % 32u;
    Q6Lane r;
    r.ql_off = jb * 64u + l + ((quad & 1u) != 0u ? 32u : 0u);
    r.qh_off = 128u + jb * 32u + l;
    r.sc_off = 192u + (jb * 128u + quad * 32u + l) / 16u;
    r.ql_shift = (quad >= 2u) ? 4u : 0u;
    r.qh_shift = quad * 2u;
    return r;
}

Q6Lane q6_lane(uint lane) {
    return q6_lane_at(lane * 4u);
}

Quad q6k_quad(U32Buf w, uint blk, Q6Lane L) {
    float d = half_at_bytes(w, blk + 208u);
    uint ql4 = bytes4_at(w, blk + L.ql_off) >> L.ql_shift;
    uint qh4 = bytes4_at(w, blk + L.qh_off) >> L.qh_shift;
    float s = float(sbyte_at(w, blk + L.sc_off));
    Quad r;
    r.q = vec4(
        float(((ql4 & 0xFu) | ((qh4 & 3u) << 4u))),
        float((((ql4 >> 8u) & 0xFu) | (((qh4 >> 8u) & 3u) << 4u))),
        float((((ql4 >> 16u) & 0xFu) | (((qh4 >> 16u) & 3u) << 4u))),
        float((((ql4 >> 24u) & 0xFu) | (((qh4 >> 24u) & 3u) << 4u)))) - vec4(32.0);
    r.d = d * s;
    r.m = 0.0;
    return r;
}

float quad_dot(Quad Q, vec4 xv) {
    return Q.d * dot(Q.q, xv) - Q.m * (xv.x + xv.y + xv.z + xv.w);
}

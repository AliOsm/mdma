// One element of a GGML Q5_K superblock (176 bytes).
float q5k_elem(U32Buf w, uint blk, uint e) {
    uint j = e / 32u, l = e % 32u;
    uint sc = blk + 4u;
    uint scale, mn;
    if (j < 4u) {
        scale = byte_at(w, sc + j) & 63u;
        mn = byte_at(w, sc + j + 4u) & 63u;
    } else {
        scale = (byte_at(w, sc + j + 4u) & 15u) | ((byte_at(w, sc + j - 4u) >> 6u) << 4u);
        mn = (byte_at(w, sc + j + 4u) >> 4u) | ((byte_at(w, sc + j) >> 6u) << 4u);
    }
    uint lo = (byte_at(w, blk + 48u + (j / 2u) * 32u + l) >> (4u * (j & 1u))) & 15u;
    uint hi = (byte_at(w, blk + 16u + l) >> j) & 1u;
    return half_at_bytes(w, blk) * float(scale) * float(lo + 16u * hi)
        - half_at_bytes(w, blk + 2u) * float(mn);
}

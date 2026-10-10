// Adapted from llama.cpp 70c4e1582, mul_mmq_cm1.comp and its helpers.
// Copyright (c) 2023-2026 The ggml authors. MIT; see LICENSE-llama.cpp.
void stage_a(U32Buf w, uint row, uint dst_row, uint loadr, uint k0) {
    uint row_byte = row * (pc.n_in / 256u) * pc.block_bytes;
    [[unroll]] for (uint ks = 0u; ks < STEP; ks++) {
        uint elem = k0 + ks * 32u + loadr * 8u;
        uint blk = row_byte + (elem / 256u) * pc.block_bytes;
        uint e0 = elem % 256u;
#ifdef QUANT_Q4K
        Q4Lane l = q4_lane_at(e0);
        uint q0 = (w.v[blk / 4u + l.qw] >> l.shift) & 0x0F0F0F0Fu;
        uint q1 = (w.v[blk / 4u + l.qw + 1u] >> l.shift) & 0x0F0F0F0Fu;
        aq[dst_row * PITCH + ks * 8u + loadr * 2u] = q0;
        aq[dst_row * PITCH + ks * 8u + loadr * 2u + 1u] = q1;
        if (loadr == 0u) {
            Quad a = q4k_quad(w, blk / 4u, l);
            adm[ks * BM + dst_row] = vec2(a.d, -a.m);
        }
#else
        Q6Lane l = q6_lane_at(e0);
        uint ql0 = bytes4_at(w, blk + l.ql_off) >> l.ql_shift;
        uint ql1 = bytes4_at(w, blk + l.ql_off + 4u) >> l.ql_shift;
        uint qh0 = bytes4_at(w, blk + l.qh_off) >> l.qh_shift;
        uint qh1 = bytes4_at(w, blk + l.qh_off + 4u) >> l.qh_shift;
        uint q0 = (ql0 & 0x0F0F0F0Fu) | ((qh0 & 0x03030303u) << 4u);
        uint q1 = (ql1 & 0x0F0F0F0Fu) | ((qh1 & 0x03030303u) << 4u);
        // Lane-isolated subtraction: no borrow crosses a packed byte.
        q0 = ((q0 | 0x80808080u) - 0x20202020u) ^ 0x80808080u;
        q1 = ((q1 | 0x80808080u) - 0x20202020u) ^ 0x80808080u;
        aq[dst_row * PITCH + ks * 8u + loadr * 2u] = q0;
        aq[dst_row * PITCH + ks * 8u + loadr * 2u + 1u] = q1;
        if (loadr % 2u == 0u) {
            ad[(ks * 2u + loadr / 2u) * BM + dst_row] =
                half_at_bytes(w, blk + 208u) * float(sbyte_at(w, blk + l.sc_off));
        }
#endif
    }
}

// Adapted from llama.cpp 70c4e1582, mul_mmq_cm1.comp and its helpers.
// Copyright (c) 2023-2026 The ggml authors. MIT; see LICENSE-llama.cpp.
#extension GL_EXT_control_flow_attributes : enable
#extension GL_EXT_shader_explicit_arithmetic_types_int8 : require
#extension GL_EXT_shader_explicit_arithmetic_types_int32 : require
#extension GL_KHR_shader_subgroup_basic : require
#extension GL_KHR_cooperative_matrix : require
#extension GL_KHR_memory_scope_semantics : enable
#include "common.glsl"
#include "kquant.glsl"
layout(local_size_x = 128) in;
layout(constant_id = 0) const uint RDNA4 = 0;
layout(push_constant, std430) uniform PC {
    uint64_t w; uint64_t x; uint64_t y;
    uint n_out; uint n_in; uint n_tokens; uint dtype; uint block_bytes;
} pc;

const uint BM = 64u, BN = 64u, WM = 32u, WN = 32u;
const uint STEP = 4u, PITCH = STEP * 8u + 4u;
shared uint aq[BM * PITCH];
shared uint bq[BN * PITCH];
#ifdef QUANT_Q4K
shared vec2 adm[STEP * BM];
#else
shared float ad[STEP * 2u * BM];
#endif
shared vec2 bds[STEP * BN];

// RDNA3 fragments interleave rows; RDNA4 fragments have two contiguous halves.
uint elem_row(uint e) {
    uint half_row = gl_SubgroupInvocationID / 16u;
    return RDNA4 != 0u ? e + half_row * 8u : half_row + 2u * e;
}

#include "int8_weight_tile.glsl"

void main() {
    uint lane = gl_LocalInvocationID.x;
    uint row0 = gl_WorkGroupID.x * BM;
    uint tok0 = gl_WorkGroupID.y * BN;
    uint ar = lane / 4u, ak = lane % 4u;
    uint br = lane / 2u, bk = lane % 2u;
    uint warp_r = gl_SubgroupID % 2u;
    uint warp_c = gl_SubgroupID / 2u;
    uint a0 = warp_r * WM, b0 = warp_c * WN;
    uint col = gl_SubgroupInvocationID % 16u;
    U32Buf w = U32Buf(pc.w), x = U32Buf(pc.x);
    float sums[32];
    [[unroll]] for (uint i = 0u; i < 32u; i++) sums[i] = 0.0;

    for (uint k0 = 0u; k0 < pc.n_in; k0 += STEP * 32u) {
        [[unroll]] for (uint i = 0u; i < 2u; i++) {
            uint r = ar + i * 32u;
            uint row = min(row0 + r, pc.n_out - 1u);
            // Clamp global loads, not the destination LDS row.
            stage_a(w, row, r, ak, k0);
        }
        [[unroll]] for (uint ks = 0u; ks < STEP; ks++) {
            uint t = min(tok0 + br, pc.n_tokens - 1u);
            uint ib = (t * (pc.n_in / 32u) + k0 / 32u + ks) * 10u;
            [[unroll]] for (uint j = 0u; j < 4u; j++)
                bq[br * PITCH + ks * 8u + bk * 4u + j] = x.v[ib + bk * 4u + j];
            if (bk == 0u)
                bds[ks * BN + br] = vec2(uintBitsToFloat(x.v[ib + 8u]), float(int(x.v[ib + 9u])));
        }
        barrier();
        [[unroll]] for (uint ks = 0u; ks < STEP; ks++) {
#ifdef QUANT_Q4K
            coopmat<int8_t, gl_ScopeSubgroup, 16, 16, gl_MatrixUseA> ca[4];
            coopmat<int8_t, gl_ScopeSubgroup, 16, 16, gl_MatrixUseB> cb[4];
            [[unroll]] for (uint r = 0u; r < 2u; r++)
                [[unroll]] for (uint h = 0u; h < 2u; h++)
                    coopMatLoad(ca[r * 2u + h], aq, (a0 + r * 16u) * PITCH + ks * 8u + h * 4u, PITCH, gl_CooperativeMatrixLayoutRowMajor);
            [[unroll]] for (uint c = 0u; c < 2u; c++)
                [[unroll]] for (uint h = 0u; h < 2u; h++)
                    coopMatLoad(cb[c * 2u + h], bq, (b0 + c * 16u) * PITCH + ks * 8u + h * 4u, PITCH, gl_CooperativeMatrixLayoutColumnMajor);
            [[unroll]] for (uint r = 0u; r < 2u; r++) {
                [[unroll]] for (uint c = 0u; c < 2u; c++) {
                    coopmat<int32_t, gl_ScopeSubgroup, 16, 16, gl_MatrixUseAccumulator> acc =
                        coopmat<int32_t, gl_ScopeSubgroup, 16, 16, gl_MatrixUseAccumulator>(0);
                    [[unroll]] for (uint h = 0u; h < 2u; h++)
                        acc = coopMatMulAdd(ca[r * 2u + h], cb[c * 2u + h], acc);
                    vec2 ds = bds[ks * BN + b0 + c * 16u + col];
                    [[unroll]] for (uint e = 0u; e < 8u; e++) {
                        vec2 dm = adm[ks * BM + a0 + r * 16u + elem_row(e)];
                        uint idx = (r * 2u + c) * 8u + e;
                        sums[idx] += fma(float(acc[e]) * dm.x, ds.x, dm.y * ds.x * ds.y);
                    }
                }
            }
#else
            [[unroll]] for (uint h = 0u; h < 2u; h++) {
                [[unroll]] for (uint r = 0u; r < 2u; r++) {
                    coopmat<int8_t, gl_ScopeSubgroup, 16, 16, gl_MatrixUseA> ca;
                    coopMatLoad(ca, aq, (a0 + r * 16u) * PITCH + ks * 8u + h * 4u, PITCH, gl_CooperativeMatrixLayoutRowMajor);
                    [[unroll]] for (uint c = 0u; c < 2u; c++) {
                        coopmat<int8_t, gl_ScopeSubgroup, 16, 16, gl_MatrixUseB> cb;
                        coopMatLoad(cb, bq, (b0 + c * 16u) * PITCH + ks * 8u + h * 4u, PITCH, gl_CooperativeMatrixLayoutColumnMajor);
                        coopmat<int32_t, gl_ScopeSubgroup, 16, 16, gl_MatrixUseAccumulator> acc =
                            coopmat<int32_t, gl_ScopeSubgroup, 16, 16, gl_MatrixUseAccumulator>(0);
                        acc = coopMatMulAdd(ca, cb, acc);
                        float db = bds[ks * BN + b0 + c * 16u + col].x;
                        [[unroll]] for (uint e = 0u; e < 8u; e++) {
                            float da = ad[(ks * 2u + h) * BM + a0 + r * 16u + elem_row(e)];
                            uint idx = (r * 2u + c) * 8u + e;
                            sums[idx] += float(acc[e]) * da * db;
                        }
                    }
                }
            }
#endif
        }
        barrier();
    }
    F32Buf y = F32Buf(pc.y);
    [[unroll]] for (uint r = 0u; r < 2u; r++)
        [[unroll]] for (uint c = 0u; c < 2u; c++)
            [[unroll]] for (uint e = 0u; e < 8u; e++) {
                uint row = row0 + a0 + r * 16u + elem_row(e);
                uint tok = tok0 + b0 + c * 16u + col;
                if (row < pc.n_out && tok < pc.n_tokens)
                    y.v[tok * pc.n_out + row] = sums[(r * 2u + c) * 8u + e];
            }
}

#extension GL_EXT_control_flow_attributes : enable
#extension GL_EXT_integer_dot_product : require
#include "common.glsl"
#include "kquant.glsl"
layout(local_size_x = 256) in;
layout(push_constant, std430) uniform PC {
    uint64_t w; uint64_t x; uint64_t y;
    uint n_out; uint n_in; uint n_tokens; uint dtype; uint block_bytes;
} pc;

const uint BM = 64u, BN = 32u, STEP = 4u, PITCH = STEP * 8u + 1u;
shared uint aq[BM * PITCH];
shared uint bq[BN * PITCH];
#ifdef QUANT_Q4K
shared vec2 adm[STEP * BM];
#else
shared float ad[STEP * 2u * BM];
#endif
shared vec2 bds[STEP * BN];
#include "int8_weight_tile.glsl"

void main() {
    uint lane = gl_LocalInvocationID.x;
    uint row0 = gl_WorkGroupID.x * BM, tok0 = gl_WorkGroupID.y * BN;
    uint ar = lane / 4u, ak = lane % 4u;
    uint br = lane / 8u, bk = lane % 8u;
    uint a0 = lane % 16u, b0 = lane / 16u;
    U32Buf w = U32Buf(pc.w), x = U32Buf(pc.x);
    float sums[8];
    [[unroll]] for (uint i = 0u; i < 8u; i++) sums[i] = 0.0;

    for (uint k0 = 0u; k0 < pc.n_in; k0 += STEP * 32u) {
        stage_a(w, min(row0 + ar, pc.n_out - 1u), ar, ak, k0);
        [[unroll]] for (uint ks = 0u; ks < STEP; ks++) {
            uint t = min(tok0 + br, pc.n_tokens - 1u);
            uint ib = (t * (pc.n_in / 32u) + k0 / 32u + ks) * 10u;
            bq[br * PITCH + ks * 8u + bk] = x.v[ib + bk];
            if (bk == 0u)
                bds[ks * BN + br] = vec2(uintBitsToFloat(x.v[ib + 8u]), float(int(x.v[ib + 9u])));
        }
        barrier();
        [[unroll]] for (uint ks = 0u; ks < STEP; ks++) {
            [[unroll]] for (uint r = 0u; r < 4u; r++) {
                uint row = a0 + r * 16u;
                [[unroll]] for (uint c = 0u; c < 2u; c++) {
                    uint tok = b0 + c * 16u;
                    vec2 ds = bds[ks * BN + tok];
#ifdef QUANT_Q4K
                    int acc = 0;
                    [[unroll]] for (uint j = 0u; j < 8u; j++)
                        acc += dotPacked4x8EXT(int(aq[row * PITCH + ks * 8u + j]),
                            int(bq[tok * PITCH + ks * 8u + j]));
                    vec2 dm = adm[ks * BM + row];
                    sums[r * 2u + c] += fma(float(acc) * dm.x, ds.x, dm.y * ds.x * ds.y);
#else
                    [[unroll]] for (uint h = 0u; h < 2u; h++) {
                        int acc = 0;
                        [[unroll]] for (uint j = 0u; j < 4u; j++)
                            acc += dotPacked4x8EXT(int(aq[row * PITCH + ks * 8u + h * 4u + j]),
                                int(bq[tok * PITCH + ks * 8u + h * 4u + j]));
                        float da = ad[(ks * 2u + h) * BM + row];
                        sums[r * 2u + c] += float(acc) * da * ds.x;
                    }
#endif
                }
            }
        }
        barrier();
    }
    F32Buf y = F32Buf(pc.y);
    [[unroll]] for (uint r = 0u; r < 4u; r++)
        [[unroll]] for (uint c = 0u; c < 2u; c++) {
            uint row = row0 + a0 + r * 16u, tok = tok0 + b0 + c * 16u;
            if (row < pc.n_out && tok < pc.n_tokens)
                y.v[tok * pc.n_out + row] = sums[r * 2u + c];
        }
}

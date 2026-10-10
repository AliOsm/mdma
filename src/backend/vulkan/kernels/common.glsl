// Shared by every kernel: buffers are addressed by device address from
// push constants (no descriptor sets), and quantized weights are read as
// bytes out of 32-bit words at absolute byte offsets, so odd row starts
// (Q6_K's 210-byte blocks) need no alignment.
// Launch sizes and ABI tags are supplied by build.rs from the host's Rust constants.
#extension GL_EXT_buffer_reference : require
#extension GL_EXT_buffer_reference2 : require
#extension GL_EXT_shader_explicit_arithmetic_types_int64 : require

layout(buffer_reference, std430, buffer_reference_align = 4) buffer F32Buf { float v[]; };
layout(buffer_reference, std430, buffer_reference_align = 4) buffer U32Buf { uint v[]; };
layout(buffer_reference, std430, buffer_reference_align = 4) buffer I32Buf { int v[]; };

uint byte_at(U32Buf b, uint i) {
    return (b.v[i >> 2] >> ((i & 3u) * 8u)) & 0xFFu;
}

// Signed byte.
int sbyte_at(U32Buf b, uint i) {
    return int(byte_at(b, i) << 24) >> 24;
}

// f16 at byte offset i (2-byte aligned).
float half_at_bytes(U32Buf b, uint i) {
    uint w = b.v[i >> 2];
    vec2 h = unpackHalf2x16(w);
    return ((i & 2u) == 0u) ? h.x : h.y;
}

// f16 element e of an array that starts at byte 0.
float half_elem(U32Buf b, uint e) {
    return half_at_bytes(b, e * 2u);
}

float bf16_elem(U32Buf b, uint e) {
    uint bits = (b.v[e >> 1u] >> ((e & 1u) * 16u)) & 0xFFFFu;
    return uintBitsToFloat(bits << 16u);
}

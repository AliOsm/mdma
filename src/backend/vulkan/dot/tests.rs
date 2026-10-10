use super::super::{VulkanBackend, coopmat::tests as shared};

fn gpu() -> VulkanBackend {
    VulkanBackend::new().unwrap().with_int8_dot(true)
}

#[test]
fn packed_dot_selection_and_disabling_are_explicit() {
    let gpu = gpu();
    assert_eq!(gpu.int8(), gpu.int8_dot_available());
    assert_eq!(gpu.int8_dot(), gpu.int8_dot_available());
    gpu.set_int8(true);
    assert_eq!(gpu.int8(), gpu.int8_available());
    assert_eq!(gpu.int8_dot(), !gpu.int8_coopmat_available() && gpu.int8_dot_available());
    gpu.set_int8_dot(false);
    assert!(!gpu.int8());
    gpu.set_int8_dot(true);
    gpu.set_int8(false);
    assert!(!gpu.int8_dot());
    assert!(!gpu.int8());
}

#[test]
fn packed_dot_matmul_matches_requantized_cpu_across_partial_tiles() {
    let gpu = gpu();
    if !gpu.int8_dot_available() {
        eprintln!("accelerated packed signed 4x8 dot products unavailable");
        return;
    }
    assert!(gpu.int8_dot());
    shared::matmul_cases(&gpu);
}

#[test]
fn packed_dot_keeps_decode_and_other_weights_on_existing_paths() {
    let gpu = gpu().with_f16_weights(false);
    shared::fallback_cases(&gpu);
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF and accelerated packed-dot Vulkan hardware"]
fn packed_dot_matches_requantized_cpu_on_real_rows() {
    let gpu = gpu();
    assert!(gpu.int8_dot(), "requires accelerated packed signed 4x8 dot products");
    shared::real_rows(&gpu);
}

#[test]
#[ignore = "requires MDMA_TEST_GGUF and accelerated packed-dot Vulkan hardware"]
fn packed_dot_prefill_then_greedy_matches_llama_cpp() {
    let gpu = gpu();
    assert!(gpu.int8_dot(), "requires accelerated packed signed 4x8 dot products");
    super::super::tests::prefill_greedy(&gpu);
}

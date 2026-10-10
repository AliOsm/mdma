use super::*;

#[cfg(feature = "cuda")]
#[test]
fn cuda_decoder_attention_matches_cpu() {
    check(&crate::backend::cuda::CudaBackend::new().unwrap());
}

#[cfg(feature = "vulkan")]
#[test]
fn vulkan_decoder_attention_matches_cpu() {
    check(&crate::backend::vulkan::VulkanBackend::new().unwrap());
}

#[cfg(feature = "rocm")]
#[test]
fn rocm_decoder_attention_matches_cpu() {
    check(&crate::backend::rocm::HipBackend::new().unwrap());
    check(&crate::backend::rocm::HipBackend::new_f32().unwrap());
}

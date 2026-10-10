//! LLM inference with GGUF weights, pluggable compute backends and an mTLS gRPC service.

pub mod gguf;
pub mod cache;
pub mod tokenizer;
pub mod model;
pub mod inference;
pub mod memory_bench;
pub mod kv_cache;
pub mod memory;
pub mod sampler;
pub mod backend;
pub mod session;
pub mod grpc;
pub mod federation;
pub mod singleton;
pub mod power;
pub mod laya;
pub mod registry;
mod artifact;
pub mod upstream;

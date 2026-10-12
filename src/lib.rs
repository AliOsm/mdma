//! LLM inference with GGUF weights, pluggable compute backends and an mTLS gRPC service.

mod artifact;
pub mod backend;
pub mod cache;
pub mod federation;
pub mod gguf;
pub mod grpc;
pub mod host;
pub mod inference;
pub mod kv_cache;
pub mod laya;
pub mod memory;
pub mod memory_bench;
pub mod model;
pub mod power;
pub mod registry;
pub mod sampler;
pub mod session;
pub mod singleton;
pub mod tokenizer;

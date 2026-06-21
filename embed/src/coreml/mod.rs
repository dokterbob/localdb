//! Safe Rust wrappers over `objc2-core-ml` for in-process CoreML inference.
//!
//! This module is gated on `#[cfg(all(target_os = "macos", feature = "local-coreml"))]`.
//! It exposes only the types needed by the CoreML embedder:
//!
//! - [`runtime::CoreMlModel`] — a loaded, compiled `.mlmodelc` or pre-compiled model.
//! - [`runtime::MlArray`] — a heap-allocated [`MLMultiArray`] wrapper for I32 and F16 data.
//! - [`runtime::Outputs`] — thin wrapper over the `MLFeatureProvider` returned by prediction.
//! - [`runtime::ComputeUnits`] — which hardware back-ends CoreML may use.
//! - [`download`] — async bundle fetch via `hf-hub`.

pub(crate) mod download;
pub(crate) mod runtime;

#[allow(unused_imports)]
pub(crate) use runtime::{ComputeUnits, CoreMlModel, MlArray, Outputs};

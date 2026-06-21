//! Safe Rust wrappers over `objc2-core-ml` types.
//!
//! # Threading model
//!
//! `MLModel` is documented by Apple as safe to call from multiple threads when
//! `predictionFromFeatures:error:` is used (each call is synchronous and
//! independent). The Obj-C runtime's autorelease pool management is also
//! thread-safe. We therefore implement `Send + Sync` for [`CoreMlModel`] with
//! an explicit SAFETY comment at the impl site.
//!
//! `MLMultiArray` is allocated fresh for every call and never shared across
//! threads, so `MlArray` only needs `Send`.
//!
//! # Unsafe budget
//!
//! All `unsafe` in this file is in clearly-labelled blocks with `// SAFETY:`
//! comments. The three categories are:
//! 1. Obj-C message sends (every `objc2` method is `unsafe`).
//! 2. Raw pointer cast from `dataPointer()` to write I32 / F16 scalars into
//!    the uninitialized `MLMultiArray` backing buffer.
//! 3. `Send + Sync` impl for `CoreMlModel`.
//!
//! # Stage note
//!
//! This module is Stage 2 of the CoreML backend: it provides the safe wrapper
//! layer only. The embedder that consumes these types will be added in Stage 3,
//! at which point the dead-code allowances below can be removed.

// dataPointer() is deprecated upstream in favour of getMutableBytesWithHandler:
// (which requires block2). We keep dataPointer() because it avoids the block2
// closure machinery and is simpler for this bulk-write use case. A follow-up
// can migrate to getMutableBytesWithHandler: if block2 is added as a dep.
#![allow(deprecated)]
// Stage 2: types are defined here and consumed in Stage 3. Suppress dead-code
// noise until the consumer (CoreMlEmbedder) is wired up.
#![allow(dead_code)]

use std::path::Path;

use half::f16;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::AnyThread;
use objc2_core_ml::{
    MLComputeUnits, MLDictionaryFeatureProvider, MLFeatureProvider, MLFeatureValue, MLModel,
    MLModelConfiguration, MLMultiArray, MLMultiArrayDataType,
};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString, NSURL};

use crate::error::EmbedError;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Which hardware back-ends CoreML may use when running the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComputeUnits {
    /// CPU only (predictable latency, works everywhere).
    CpuOnly,
    /// CPU + Apple Neural Engine (recommended for embedding models on Apple Silicon).
    CpuAndNeuralEngine,
    /// CPU + GPU.
    CpuAndGpu,
    /// All available units (CoreML chooses at load time).
    All,
}

impl ComputeUnits {
    fn to_ml(self) -> MLComputeUnits {
        match self {
            Self::CpuOnly => MLComputeUnits::CPUOnly,
            Self::CpuAndNeuralEngine => MLComputeUnits::CPUAndNeuralEngine,
            Self::CpuAndGpu => MLComputeUnits::CPUAndGPU,
            Self::All => MLComputeUnits::All,
        }
    }
}

// ---------------------------------------------------------------------------
// CoreMlModel
// ---------------------------------------------------------------------------

/// A loaded CoreML model.
///
/// Wraps `Retained<MLModel>`. `MLModel` is documented as thread-safe for
/// concurrent `predictionFromFeatures:error:` calls; see the SAFETY comment on
/// the `Send + Sync` impl below.
pub struct CoreMlModel {
    inner: Retained<MLModel>,
}

// SAFETY: Apple's CoreML documentation states that `MLModel` is safe to use
// from multiple threads when using the synchronous prediction API
// (`predictionFromFeatures:error:`). Each prediction call is independent and
// does not mutate shared mutable state visible across threads. The Obj-C
// runtime's retain/release operations are also thread-safe. We therefore
// assert `Send + Sync` for `CoreMlModel`.
unsafe impl Send for CoreMlModel {}
unsafe impl Sync for CoreMlModel {}

impl CoreMlModel {
    /// Load a compiled `.mlmodelc` directory from `model_path`.
    ///
    /// Pass a path to a pre-compiled model directory (the output of
    /// `coremltools.models.MLModel.save()` or `xcrun coremlcompiler compile`).
    ///
    /// `compute_units` selects which hardware back-ends CoreML may use.
    pub fn load(model_path: &Path, compute_units: ComputeUnits) -> Result<Self, EmbedError> {
        let path_str = model_path
            .to_str()
            .ok_or_else(|| EmbedError::Internal("model path is not valid UTF-8".to_string()))?;

        let ns_path = NSString::from_str(path_str);
        let url = NSURL::fileURLWithPath(&ns_path);

        // SAFETY: MLModelConfiguration::new() allocates a default
        // configuration object. setComputeUnits: mutates it before the model
        // is loaded, which is safe because `config` is exclusively owned here.
        let config = unsafe {
            let config = MLModelConfiguration::new();
            config.setComputeUnits(compute_units.to_ml());
            config
        };

        // SAFETY: modelWithContentsOfURL:configuration:error: is an Obj-C
        // class method that synchronously loads the compiled model from disk.
        // On failure it returns nil + an NSError, which objc2 maps to Err.
        let model = unsafe {
            MLModel::modelWithContentsOfURL_configuration_error(&url, &config)
                .map_err(|e| EmbedError::ModelMissing(format!("CoreML load error: {e:?}")))?
        };

        Ok(Self { inner: model })
    }

    /// Run a synchronous prediction from a set of named input arrays.
    ///
    /// Builds an `MLDictionaryFeatureProvider` internally from the `(name,
    /// array)` pairs, so callers never touch the Obj-C feature-provider types.
    /// Returns an [`Outputs`] wrapping the result feature provider.
    pub fn predict(&self, inputs: &[(&str, MlArray)]) -> Result<Outputs, EmbedError> {
        let provider = build_feature_provider(inputs)?;
        self.predict_provider(&provider)
    }

    /// Run predictions for several input sets, returning one [`Outputs`] each.
    ///
    /// Falls back to a sequential loop over [`predict`][Self::predict] because
    /// the `MLArrayBatchProvider` batch API requires constructing a concrete
    /// `MLArrayBatchProvider` that is complex and adds overhead for small
    /// batches typical in embedding workloads.
    pub fn predict_batch(
        &self,
        batches: &[Vec<(&str, MlArray)>],
    ) -> Result<Vec<Outputs>, EmbedError> {
        batches.iter().map(|inp| self.predict(inp)).collect()
    }

    /// Run a synchronous prediction from a pre-built feature provider.
    fn predict_provider(&self, input: &MLDictionaryFeatureProvider) -> Result<Outputs, EmbedError> {
        // SAFETY: predictionFromFeatures:error: is a synchronous Obj-C method
        // that takes a reference to an MLFeatureProvider. MLDictionaryFeatureProvider
        // is a concrete Obj-C class (implements Message) that conforms to the
        // MLFeatureProvider protocol, so ProtocolObject::from_ref succeeds.
        // The returned feature provider is retained for the lifetime of Outputs.
        let output = unsafe {
            let input_prot: &ProtocolObject<dyn MLFeatureProvider> =
                ProtocolObject::from_ref(input);
            self.inner
                .predictionFromFeatures_error(input_prot)
                .map_err(|e| EmbedError::Internal(format!("CoreML prediction error: {e:?}")))?
        };
        Ok(Outputs { inner: output })
    }
}

/// Build an `MLDictionaryFeatureProvider` from named [`MlArray`] inputs.
///
/// All `unsafe` Obj-C interaction (wrapping each multi-array in an
/// `MLFeatureValue`, erasing it to `AnyObject`, building the `NSDictionary`,
/// and constructing the provider) is contained here so the embedder layer can
/// stay free of `objc2` imports.
fn build_feature_provider(
    inputs: &[(&str, MlArray)],
) -> Result<Retained<MLDictionaryFeatureProvider>, EmbedError> {
    let keys: Vec<Retained<NSString>> = inputs
        .iter()
        .map(|(name, _)| NSString::from_str(name))
        .collect();

    // Wrap each MLMultiArray in an MLFeatureValue and erase it to AnyObject so
    // the dictionary matches `initWithDictionary:error:`'s expected type.
    let values: Vec<Retained<AnyObject>> = inputs
        .iter()
        .map(|(_, arr)| {
            // SAFETY: featureValueWithMultiArray: wraps a retained MLMultiArray
            // in a new autoreleased MLFeatureValue; objc2 retains it. The array
            // outlives this call (it is owned by `inputs`).
            let fv = unsafe { MLFeatureValue::featureValueWithMultiArray(&arr.inner) };
            // NSObject -> AnyObject: MLFeatureValue's super is NSObject, whose
            // super is AnyObject; both casts are layout-compatible upcasts.
            fv.into_super().into_super()
        })
        .collect();

    let key_refs: Vec<&NSString> = keys.iter().map(|k| k.as_ref()).collect();
    let dict: Retained<NSDictionary<NSString, AnyObject>> =
        NSDictionary::from_retained_objects(&key_refs, &values);

    // SAFETY: initWithDictionary:error: constructs a feature provider from a
    // dictionary whose values are all MLFeatureValue instances (erased to
    // AnyObject above). It returns nil + NSError if a value cannot be
    // represented, which objc2 maps to Err.
    let provider = unsafe {
        MLDictionaryFeatureProvider::initWithDictionary_error(
            MLDictionaryFeatureProvider::alloc(),
            &dict,
        )
        .map_err(|e| EmbedError::Internal(format!("build feature provider: {e:?}")))?
    };
    Ok(provider)
}

// ---------------------------------------------------------------------------
// MlArray
// ---------------------------------------------------------------------------

/// A wrapper around `MLMultiArray` with typed constructors.
///
/// The backing buffer is allocated by CoreML and initialised via
/// `dataPointer()`. This is the simplest path for writing data without
/// requiring the `block2` feature's closure-based `getMutableBytesWithHandler:`.
pub struct MlArray {
    pub(crate) inner: Retained<MLMultiArray>,
}

impl MlArray {
    /// Build an `NSArray<NSNumber>` shape descriptor (one `NSNumber` per dim).
    ///
    /// CoreML validates input arrays against the model's declared rank, so the
    /// shape must match the model exactly (e.g. `(1, L)` rank-2, not `(L,)`).
    fn ns_shape(shape: &[usize]) -> objc2::rc::Retained<NSArray<NSNumber>> {
        let dims: Vec<objc2::rc::Retained<NSNumber>> = shape
            .iter()
            .map(|&dim| NSNumber::numberWithInteger(dim as isize))
            .collect();
        NSArray::from_retained_slice(&dims)
    }

    /// Allocate an `MLMultiArray` of `MLMultiArrayDataTypeInt32` with the given
    /// (row-major) `shape` and fill it with the provided `i32` values.
    ///
    /// `data` must hold exactly `shape.iter().product()` elements; the contiguous
    /// row-major buffer is written verbatim.
    pub fn int32(shape: &[usize], data: &[i32]) -> Result<Self, EmbedError> {
        debug_assert_eq!(
            data.len(),
            shape.iter().product::<usize>(),
            "int32 data length must equal the product of the shape dims"
        );
        let ns_shape = Self::ns_shape(shape);

        // SAFETY: initWithShape:dataType:error: allocates an MLMultiArray with
        // an uninitialized backing buffer of `count * sizeof(i32)` bytes (where
        // `count` is the product of `shape`'s dims). On failure it returns
        // nil + NSError.
        let array = unsafe {
            MLMultiArray::initWithShape_dataType_error(
                MLMultiArray::alloc(),
                &ns_shape,
                MLMultiArrayDataType::Int32,
            )
            .map_err(|e| EmbedError::Internal(format!("MLMultiArray(i32) alloc: {e:?}")))?
        };

        // SAFETY: `dataPointer()` returns a non-null pointer to the backing
        // buffer. The buffer is at least `data.len() * 4` bytes (Int32 = 4
        // bytes/elem) because `data.len()` equals the product of the shape dims.
        // We hold the sole reference to `array` — no other thread or Obj-C
        // code can alias this buffer while we write. We write exactly
        // `data.len()` i32 values which equals the total element count.
        unsafe {
            let ptr = array.dataPointer().as_ptr() as *mut i32;
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
        }

        Ok(Self { inner: array })
    }

    /// Allocate an `MLMultiArray` of `MLMultiArrayDataTypeFloat16` with the
    /// given (row-major) `shape` and fill it with the provided `f16` values
    /// (using the `half` crate).
    ///
    /// `data` must hold exactly `shape.iter().product()` elements.
    pub fn f16(shape: &[usize], data: &[f16]) -> Result<Self, EmbedError> {
        debug_assert_eq!(
            data.len(),
            shape.iter().product::<usize>(),
            "f16 data length must equal the product of the shape dims"
        );
        let ns_shape = Self::ns_shape(shape);

        // SAFETY: Allocates a Float16 backing buffer of `count * 2` bytes (where
        // `count` is the product of `shape`'s dims).
        let array = unsafe {
            MLMultiArray::initWithShape_dataType_error(
                MLMultiArray::alloc(),
                &ns_shape,
                MLMultiArrayDataType::Float16,
            )
            .map_err(|e| EmbedError::Internal(format!("MLMultiArray(f16) alloc: {e:?}")))?
        };

        // SAFETY: `dataPointer()` returns the backing buffer pointer. Float16
        // scalars are 2-byte IEEE 754 half-precision values; `half::f16` is
        // `repr(transparent)` over `u16` and has the same layout. We copy
        // `data.len()` elements = `data.len() * 2` bytes into a buffer
        // of at least that size (`data.len()` equals the product of the shape
        // dims). No aliasing occurs because we hold the sole Retained<> handle
        // and no block or Obj-C method has a reference.
        unsafe {
            let ptr = array.dataPointer().as_ptr() as *mut f16;
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
        }

        Ok(Self { inner: array })
    }

    /// Read the underlying `MLMultiArray`'s declared shape into a `Vec<usize>`.
    ///
    /// Used by regression tests to assert the allocated array has the requested
    /// rank (e.g. rank-2 `(1, L)` rather than rank-1 `(L,)`); see the B1 fix.
    #[cfg(test)]
    pub(crate) fn shape(&self) -> Vec<usize> {
        // SAFETY: `shape()` is a read-only Obj-C property access returning an
        // `NSArray<NSNumber>` describing each dimension. We hold a Retained<>
        // handle to `inner`, so the array (and its shape NSArray) stay alive for
        // the duration of this call. Each element is an NSNumber whose
        // `integerValue` is the (non-negative) dimension size.
        unsafe {
            let dims = self.inner.shape();
            (0..dims.count())
                .map(|i| dims.objectAtIndex(i).integerValue() as usize)
                .collect()
        }
    }
}

// ---------------------------------------------------------------------------
// Outputs
// ---------------------------------------------------------------------------

/// Wrapper around the `MLFeatureProvider` returned by a CoreML prediction.
pub struct Outputs {
    inner: Retained<ProtocolObject<dyn MLFeatureProvider>>,
}

impl Outputs {
    /// Extract a named feature as a flat `Vec<f32>` by reading each element of
    /// an `MLMultiArray` output that has `MLMultiArrayDataTypeInt8` dtype and
    /// converting to `f32`.
    ///
    /// The Int8 values are sign-extended to `i8` and then widened to `f32`.
    ///
    /// Returns `EmbedError::Internal` if the feature is missing or is not a
    /// multi-array with Int8 dtype.
    pub fn int8_as_f32(&self, feature_name: &str) -> Result<Vec<f32>, EmbedError> {
        let ns_name = NSString::from_str(feature_name);

        // SAFETY: featureValueForName: is a read-only Obj-C call on a retained
        // protocol object. Returns nil (None) if the name is absent.
        let feature_value: Retained<MLFeatureValue> = unsafe {
            self.inner.featureValueForName(&ns_name).ok_or_else(|| {
                EmbedError::Internal(format!(
                    "CoreML output has no feature named '{feature_name}'"
                ))
            })?
        };

        // SAFETY: multiArrayValue returns nil if the feature is not a
        // multi-array; we return an error in that case.
        let array: Retained<MLMultiArray> = unsafe {
            feature_value.multiArrayValue().ok_or_else(|| {
                EmbedError::Internal(format!(
                    "CoreML feature '{feature_name}' is not a multi-array"
                ))
            })?
        };

        // SAFETY: dataType() is a read-only property access on a retained ref.
        let dtype = unsafe { array.dataType() };
        if dtype != MLMultiArrayDataType::Int8 {
            return Err(EmbedError::Internal(format!(
                "CoreML feature '{feature_name}' has unexpected dtype \
                 (expected Int8, got {dtype:?})"
            )));
        }

        // SAFETY: count() returns the total number of scalar elements.
        let count = unsafe { array.count() } as usize;

        // SAFETY: `dataPointer()` gives us a non-null pointer to the backing
        // buffer holding `count` Int8 scalars (1 byte each). We read `count`
        // bytes as `i8` and convert each to `f32`. The Retained<> keeps `array`
        // alive for the duration of this function, ensuring the pointer is valid.
        let result = unsafe {
            let ptr = array.dataPointer().as_ptr() as *const i8;
            (0..count)
                .map(|i| i8_to_f32(*ptr.add(i)))
                .collect::<Vec<f32>>()
        };

        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// Pure helpers (no unsafe, tested in unit tests below)
// ---------------------------------------------------------------------------

/// Convert a raw `u16` bit pattern to `f16`.
///
/// Used in tests to roundtrip F16 values through the MLMultiArray backing
/// buffer without relying on `half`'s `from_bits` being inlined.
#[inline]
pub(crate) fn f16_from_bits(bits: u16) -> f16 {
    f16::from_bits(bits)
}

/// Widen an `i8` scalar to `f32`.
#[inline]
fn i8_to_f32(v: i8) -> f32 {
    v as f32
}

// ---------------------------------------------------------------------------
// Unit tests (pure helpers only — no Obj-C runtime required)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn i8_to_f32_positive() {
        assert_eq!(i8_to_f32(42_i8), 42.0_f32);
    }

    #[test]
    fn i8_to_f32_negative() {
        assert_eq!(i8_to_f32(-1_i8), -1.0_f32);
    }

    #[test]
    fn i8_to_f32_min_max() {
        assert_eq!(i8_to_f32(i8::MIN), -128.0_f32);
        assert_eq!(i8_to_f32(i8::MAX), 127.0_f32);
    }

    #[test]
    fn f16_from_bits_zero() {
        let z = f16_from_bits(0u16);
        assert_eq!(z.to_f32(), 0.0_f32);
    }

    #[test]
    fn f16_from_bits_one() {
        // IEEE 754 half: sign=0, exponent=01111, mantissa=0000000000 → 1.0
        let one = f16_from_bits(0x3C00u16);
        assert!((one.to_f32() - 1.0_f32).abs() < 1e-6);
    }

    #[test]
    fn f16_from_bits_minus_one() {
        // sign=1, exponent=01111, mantissa=0000000000 → -1.0
        let neg_one = f16_from_bits(0xBC00u16);
        assert!((neg_one.to_f32() + 1.0_f32).abs() < 1e-6);
    }

    // ---- MlArray rank regression (B1) ----
    //
    // These allocate real MLMultiArrays (no model load required) and assert the
    // declared shape matches the requested rank-2 shape.
    // regression: MLMultiArray must be rank-2 (see B1) — CoreML rejects rank-1 inputs

    #[test]
    fn mlarray_int32_is_rank2() {
        let arr = MlArray::int32(&[1, 512], &vec![0i32; 512]).unwrap();
        assert_eq!(arr.shape(), vec![1, 512]);
        assert_eq!(arr.shape().len(), 2);
    }

    #[test]
    fn mlarray_f16_pool_is_rank2() {
        let arr = MlArray::f16(&[32, 512], &vec![f16::ZERO; 32 * 512]).unwrap();
        assert_eq!(arr.shape(), vec![32, 512]);
        // Explicit rank check so a rank-1 (len,) regression is caught directly.
        assert_eq!(arr.shape().len(), 2);
    }

    #[test]
    fn mlarray_f16_mask_is_rank2() {
        let arr = MlArray::f16(&[1, 512], &vec![f16::ZERO; 512]).unwrap();
        assert_eq!(arr.shape(), vec![1, 512]);
        assert_eq!(arr.shape().len(), 2);
    }

    #[test]
    fn compute_units_maps_to_ml() {
        // Verify the mapping table is consistent with MLComputeUnits constants.
        assert_eq!(ComputeUnits::CpuOnly.to_ml(), MLComputeUnits::CPUOnly);
        assert_eq!(
            ComputeUnits::CpuAndNeuralEngine.to_ml(),
            MLComputeUnits::CPUAndNeuralEngine
        );
        assert_eq!(ComputeUnits::CpuAndGpu.to_ml(), MLComputeUnits::CPUAndGPU);
        assert_eq!(ComputeUnits::All.to_ml(), MLComputeUnits::All);
    }
}

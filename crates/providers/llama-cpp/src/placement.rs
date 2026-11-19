//! Multi-device model placement: which backend devices a model is loaded
//! onto, in what proportions, and how it is split.
//!
//! [`ResolvedPlacement::resolve`] is a pure function over a snapshot of the
//! available devices so the validation rules can be unit-tested without a GPU.
//! The result is `Eq` so it can be part of the model cache key.

use crate::config::{LlamaCppConfig, SplitMode};
use llama_cpp_2::model::params::LlamaModelParams;
use querymt::error::LLMError;

/// The subset of a ggml backend device the resolver needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeviceInfo {
    pub index: usize,
    pub name: String,
}

/// Snapshot of the devices llama.cpp currently knows about.
///
/// Requires the llama backend to be initialized.
pub(crate) fn available_devices() -> Vec<DeviceInfo> {
    llama_cpp_2::list_llama_ggml_backend_devices()
        .into_iter()
        .map(|d| DeviceInfo {
            index: d.index,
            name: d.name,
        })
        .collect()
}

/// Validated placement settings for one model load.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ResolvedPlacement {
    /// ggml device indices, in the order given by the user. `None` leaves
    /// device selection to llama.cpp.
    pub devices: Option<Vec<usize>>,
    /// Shares as `f32::to_bits` so the struct can be `Eq` / a cache key.
    /// Empty means llama.cpp's free-memory default.
    tensor_split_bits: Vec<u32>,
    pub split_mode: Option<SplitMode>,
    pub main_gpu: Option<u32>,
    /// Device (index into `devices`) for an MTP sidecar. See [`Self::for_sidecar`].
    sidecar_main_gpu: Option<u32>,
}

fn invalid(msg: impl Into<String>) -> LLMError {
    LLMError::InvalidRequest(msg.into())
}

impl ResolvedPlacement {
    /// Whether no placement option was configured at all.
    pub(crate) fn is_default(&self) -> bool {
        self == &Self::default()
    }

    /// Validate the placement fields of `cfg` against the available devices.
    pub(crate) fn resolve(
        cfg: &LlamaCppConfig,
        available: &[DeviceInfo],
    ) -> Result<Self, LLMError> {
        let sidecar_main_gpu = cfg.speculative.as_ref().and_then(|s| s.main_gpu);

        let devices = match &cfg.devices {
            None => None,
            Some(names) => Some(resolve_devices(names, available)?),
        };

        if let Some(split) = &cfg.tensor_split {
            validate_split(split, devices.as_ref().map(Vec::len))?;
            if cfg.split_mode == Some(SplitMode::None) {
                return Err(invalid(
                    "tensor_split cannot be combined with split_mode = \"none\" \
                     (a single device holds the whole model)",
                ));
            }
        }

        if let Some(devices) = &devices {
            for (field, value) in [
                ("main_gpu", cfg.main_gpu),
                ("speculative.main_gpu", sidecar_main_gpu),
            ] {
                if let Some(v) = value
                    && v as usize >= devices.len()
                {
                    return Err(invalid(format!(
                        "{field} = {v} is out of range: `devices` lists {} device(s)",
                        devices.len()
                    )));
                }
            }
        }

        Ok(Self {
            devices,
            tensor_split_bits: cfg
                .tensor_split
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|s| s.to_bits())
                .collect(),
            split_mode: cfg.split_mode,
            main_gpu: cfg.main_gpu,
            sidecar_main_gpu,
        })
    }

    /// The tensor split shares; empty when unset.
    pub(crate) fn tensor_split(&self) -> Vec<f32> {
        self.tensor_split_bits
            .iter()
            .map(|b| f32::from_bits(*b))
            .collect()
    }

    /// Placement for an MTP sidecar model.
    ///
    /// A sidecar is a small draft model: splitting it saves little memory and
    /// adds a device hop to every draft step, so it is loaded on a single
    /// device chosen from the target's device list (`speculative.main_gpu`,
    /// falling back to the target's `main_gpu`). Without any placement
    /// configuration this is the default, preserving prior behavior.
    pub(crate) fn for_sidecar(&self) -> Self {
        if self.is_default() {
            return Self::default();
        }
        Self {
            devices: self.devices.clone(),
            tensor_split_bits: Vec::new(),
            split_mode: Some(SplitMode::None),
            main_gpu: self.sidecar_main_gpu.or(self.main_gpu),
            sidecar_main_gpu: None,
        }
    }

    /// Apply this placement to model-load params.
    pub(crate) fn apply(&self, mut params: LlamaModelParams) -> Result<LlamaModelParams, LLMError> {
        if let Some(devices) = &self.devices {
            params = params
                .with_devices(devices)
                .map_err(|e| invalid(format!("invalid devices: {e}")))?;
        }
        if !self.tensor_split_bits.is_empty() {
            params = params
                .with_tensor_split(&self.tensor_split())
                .map_err(|e| invalid(format!("invalid tensor_split: {e}")))?;
        }
        if let Some(mode) = self.split_mode {
            params = params.with_split_mode(mode.into());
        }
        if let Some(gpu) = self.main_gpu {
            params = params.with_main_gpu(i32::try_from(gpu).unwrap_or(i32::MAX));
        }
        Ok(params)
    }

    /// One-line description for the startup log.
    pub(crate) fn describe(&self) -> String {
        format!(
            "devices={:?} tensor_split={:?} split_mode={:?} main_gpu={:?}",
            self.devices,
            self.tensor_split(),
            self.split_mode,
            self.main_gpu
        )
    }
}

fn resolve_devices(names: &[String], available: &[DeviceInfo]) -> Result<Vec<usize>, LLMError> {
    if names.is_empty() {
        return Err(invalid("`devices` must not be empty when set"));
    }
    let mut indices = Vec::with_capacity(names.len());
    for name in names {
        let found = available
            .iter()
            .find(|d| d.name.eq_ignore_ascii_case(name.trim()))
            .ok_or_else(|| {
                let known: Vec<&str> = available.iter().map(|d| d.name.as_str()).collect();
                invalid(format!(
                    "unknown device {name:?}; available devices: [{}]",
                    known.join(", ")
                ))
            })?;
        if indices.contains(&found.index) {
            return Err(invalid(format!(
                "device {:?} is listed more than once",
                found.name
            )));
        }
        indices.push(found.index);
    }
    Ok(indices)
}

fn validate_split(split: &[f32], n_devices: Option<usize>) -> Result<(), LLMError> {
    if split.is_empty() {
        return Err(invalid("`tensor_split` must not be empty when set"));
    }
    if let Some(n) = n_devices
        && split.len() > n
    {
        return Err(invalid(format!(
            "`tensor_split` has {} entries but `devices` lists only {n}",
            split.len()
        )));
    }
    if split.iter().any(|s| !s.is_finite() || *s < 0.0) {
        return Err(invalid(
            "`tensor_split` shares must be finite and non-negative",
        ));
    }
    if split.iter().sum::<f32>() <= 0.0 {
        return Err(invalid("`tensor_split` shares must not all be zero"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn devs() -> Vec<DeviceInfo> {
        ["CUDA0", "CUDA1", "CPU"]
            .iter()
            .enumerate()
            .map(|(index, name)| DeviceInfo {
                index,
                name: name.to_string(),
            })
            .collect()
    }

    fn cfg(extra: &str) -> LlamaCppConfig {
        let json = if extra.is_empty() {
            r#"{"model":"m.gguf"}"#.to_string()
        } else {
            format!(r#"{{"model":"m.gguf",{extra}}}"#)
        };
        serde_json::from_str(&json).expect("valid test config")
    }

    fn err(extra: &str) -> String {
        match ResolvedPlacement::resolve(&cfg(extra), &devs()) {
            Err(LLMError::InvalidRequest(m)) => m,
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    #[test]
    fn unset_is_default() {
        let p = ResolvedPlacement::resolve(&cfg(""), &devs()).unwrap();
        assert!(p.is_default());
        assert!(p.for_sidecar().is_default());
    }

    #[test]
    fn resolves_names_case_insensitively_in_user_order() {
        let p = ResolvedPlacement::resolve(
            &cfg(r#""devices":["cuda1","CUDA0"],"tensor_split":[3,1]"#),
            &devs(),
        )
        .unwrap();
        assert_eq!(p.devices, Some(vec![1, 0]));
        assert_eq!(p.tensor_split(), vec![3.0, 1.0]);
    }

    #[test]
    fn unknown_device_lists_available() {
        let m = err(r#""devices":["Vulkan9"]"#);
        assert!(
            m.contains("Vulkan9") && m.contains("CUDA0, CUDA1, CPU"),
            "{m}"
        );
    }

    #[test]
    fn rejects_duplicates_and_empty() {
        assert!(err(r#""devices":["CUDA0","cuda0"]"#).contains("more than once"));
        assert!(err(r#""devices":[]"#).contains("must not be empty"));
        assert!(err(r#""tensor_split":[]"#).contains("must not be empty"));
    }

    #[test]
    fn rejects_bad_shares() {
        assert!(err(r#""tensor_split":[1,-1]"#).contains("non-negative"));
        assert!(err(r#""tensor_split":[0,0]"#).contains("all be zero"));
        assert!(err(r#""devices":["CUDA0"],"tensor_split":[1,1]"#).contains("lists only 1"));
    }

    #[test]
    fn split_conflicts_with_single_device_mode() {
        assert!(err(r#""tensor_split":[1,1],"split_mode":"none""#).contains("split_mode"));
    }

    #[test]
    fn main_gpu_is_range_checked_against_devices() {
        assert!(err(r#""devices":["CUDA0","CUDA1"],"main_gpu":2"#).contains("out of range"));
        assert!(
            err(r#""devices":["CUDA0"],"speculative":{"type":"mtp","main_gpu":1}"#)
                .contains("speculative.main_gpu")
        );
        assert!(
            ResolvedPlacement::resolve(
                &cfg(r#""devices":["CUDA0","CUDA1"],"main_gpu":1"#),
                &devs()
            )
            .is_ok()
        );
    }

    #[test]
    fn sidecar_is_single_device_without_split() {
        let p = ResolvedPlacement::resolve(
            &cfg(
                r#""devices":["CUDA0","CUDA1"],"tensor_split":[3,1],"main_gpu":0,
                   "speculative":{"type":"mtp","main_gpu":1}"#,
            ),
            &devs(),
        )
        .unwrap();
        let s = p.for_sidecar();
        assert_eq!(s.devices, Some(vec![0, 1]));
        assert!(s.tensor_split().is_empty());
        assert_eq!(s.split_mode, Some(SplitMode::None));
        assert_eq!(s.main_gpu, Some(1));
    }

    #[test]
    fn sidecar_falls_back_to_target_main_gpu() {
        let p = ResolvedPlacement::resolve(
            &cfg(r#""devices":["CUDA0","CUDA1"],"main_gpu":1"#),
            &devs(),
        )
        .unwrap();
        assert_eq!(p.for_sidecar().main_gpu, Some(1));
    }

    #[test]
    fn differing_splits_are_distinct_cache_keys() {
        let a = ResolvedPlacement::resolve(&cfg(r#""tensor_split":[3,1]"#), &devs()).unwrap();
        let b = ResolvedPlacement::resolve(&cfg(r#""tensor_split":[1,1]"#), &devs()).unwrap();
        assert_ne!(a, b);
        assert_eq!(a, a.clone());
    }

    #[test]
    fn split_mode_deserializes_and_rejects_unknown() {
        let p = ResolvedPlacement::resolve(&cfg(r#""split_mode":"row""#), &devs()).unwrap();
        assert_eq!(p.split_mode, Some(SplitMode::Row));
        assert!(
            serde_json::from_str::<LlamaCppConfig>(r#"{"model":"m","split_mode":"bogus"}"#)
                .is_err()
        );
    }

    #[test]
    fn apply_sets_split_mode_main_gpu_and_pads_split() {
        let p = ResolvedPlacement::resolve(
            &cfg(r#""tensor_split":[3,1],"split_mode":"row","main_gpu":1"#),
            &devs(),
        )
        .unwrap();
        let params = p.apply(LlamaModelParams::default()).unwrap();
        assert_eq!(params.main_gpu(), 1);
        assert_eq!(
            params.split_mode(),
            Ok(llama_cpp_2::model::params::LlamaSplitMode::Row)
        );
        assert_eq!(&params.tensor_split()[..2], &[3.0, 1.0]);
        assert_eq!(params.tensor_split().len(), llama_cpp_2::max_devices());
    }
}

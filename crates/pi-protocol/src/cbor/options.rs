//! Codec options, mirroring `packages/protocol/src/cbor/options.ts`.

use crate::error::CborError;

pub const UINT32_BASE: u64 = 0x1_0000_0000;
pub const MAX_UINT32: u64 = 0xffff_ffff;

/// Default upper bound for one CBOR payload (16 MiB).
pub const DEFAULT_MAX_CBOR_BYTE_LENGTH: usize = 16 * 1024 * 1024;
/// Default upper bound for array/map lengths.
pub const DEFAULT_MAX_CBOR_CONTAINER_LENGTH: usize = 1_000_000;
/// Default maximum nesting depth.
pub const DEFAULT_MAX_CBOR_DEPTH: usize = 64;
/// Pi accepts configured nesting up to this depth. Keep this finite even on
/// platforms where `usize` is wider than the JavaScript safe integer range.
const MAX_CONFIGURED_DEPTH: usize = 512;

#[derive(Debug, Clone, Default)]
pub struct CborOptions {
    pub max_byte_length: Option<usize>,
    pub max_container_length: Option<usize>,
    pub max_depth: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct ResolvedCborOptions {
    pub max_byte_length: usize,
    pub max_container_length: usize,
    pub max_depth: usize,
}

fn resolve_limit(name: &str, value: usize, maximum: usize) -> Result<usize, CborError> {
    if value > maximum {
        return Err(CborError::new(format!(
            "{name} must be an integer between 0 and {maximum}"
        )));
    }
    Ok(value)
}

pub fn resolve_options(options: &CborOptions) -> Result<ResolvedCborOptions, CborError> {
    Ok(ResolvedCborOptions {
        max_byte_length: resolve_limit(
            "maxByteLength",
            options
                .max_byte_length
                .unwrap_or(DEFAULT_MAX_CBOR_BYTE_LENGTH),
            MAX_UINT32 as usize,
        )?,
        max_container_length: resolve_limit(
            "maxContainerLength",
            options
                .max_container_length
                .unwrap_or(DEFAULT_MAX_CBOR_CONTAINER_LENGTH),
            MAX_UINT32 as usize,
        )?,
        max_depth: resolve_limit(
            "maxDepth",
            options.max_depth.unwrap_or(DEFAULT_MAX_CBOR_DEPTH),
            MAX_CONFIGURED_DEPTH,
        )?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_upstream() {
        let resolved = resolve_options(&CborOptions::default()).unwrap();
        assert_eq!(resolved.max_byte_length, 16 * 1024 * 1024);
        assert_eq!(resolved.max_container_length, 1_000_000);
        assert_eq!(resolved.max_depth, 64);
    }

    #[test]
    fn accepts_upstream_maximum_configured_depth() {
        let resolved = resolve_options(&CborOptions {
            max_depth: Some(MAX_CONFIGURED_DEPTH),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(resolved.max_depth, MAX_CONFIGURED_DEPTH);
    }

    #[test]
    fn rejects_configured_depth_above_upstream_maximum() {
        let error = resolve_options(&CborOptions {
            max_depth: Some(MAX_CONFIGURED_DEPTH + 1),
            ..Default::default()
        })
        .unwrap_err();
        assert_eq!(error.0, "maxDepth must be an integer between 0 and 512");
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn rejects_byte_and_container_limits_above_unsigned_32_bit_maximum() {
        let byte_error = resolve_options(&CborOptions {
            max_byte_length: Some(usize::MAX),
            ..Default::default()
        })
        .unwrap_err();
        assert_eq!(
            byte_error.0,
            "maxByteLength must be an integer between 0 and 4294967295"
        );

        let container_error = resolve_options(&CborOptions {
            max_container_length: Some(usize::MAX),
            ..Default::default()
        })
        .unwrap_err();
        assert_eq!(
            container_error.0,
            "maxContainerLength must be an integer between 0 and 4294967295"
        );
    }
}

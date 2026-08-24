//! Codec options, mirroring `packages/protocol/src/cbor/options.ts`.

pub const UINT32_BASE: u64 = 0x1_0000_0000;
pub const MAX_UINT32: u64 = 0xffff_ffff;

/// Default upper bound for one CBOR payload (16 MiB).
pub const DEFAULT_MAX_CBOR_BYTE_LENGTH: usize = 16 * 1024 * 1024;
/// Default upper bound for array/map lengths.
pub const DEFAULT_MAX_CBOR_CONTAINER_LENGTH: usize = 1_000_000;
/// Default maximum nesting depth.
pub const DEFAULT_MAX_CBOR_DEPTH: usize = 64;

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

pub fn resolve_options(options: &CborOptions) -> ResolvedCborOptions {
    ResolvedCborOptions {
        max_byte_length: options
            .max_byte_length
            .unwrap_or(DEFAULT_MAX_CBOR_BYTE_LENGTH),
        max_container_length: options
            .max_container_length
            .unwrap_or(DEFAULT_MAX_CBOR_CONTAINER_LENGTH),
        max_depth: options.max_depth.unwrap_or(DEFAULT_MAX_CBOR_DEPTH),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_upstream() {
        let resolved = resolve_options(&CborOptions::default());
        assert_eq!(resolved.max_byte_length, 16 * 1024 * 1024);
        assert_eq!(resolved.max_container_length, 1_000_000);
        assert_eq!(resolved.max_depth, 64);
    }
}

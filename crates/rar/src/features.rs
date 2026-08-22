//! Writer feature vocabulary.
//!
//! Mirrors the reference layout's `features` module: a small set of
//! optional archive capabilities that option-validation can reason about
//! without reaching into the format modules.

/// An optional archive capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Feature {
    /// Solid compression (members share one LZ window).
    Solid,
    /// Header encryption (`-hp`): the block stream after the per-volume
    /// plaintext header is encrypted.
    HeaderEncryption,
    /// Quick-open record (`-rr` locator + "QO" service block).
    QuickOpen,
}

impl Feature {
    /// All features.
    pub const ALL: [Feature; 3] = [
        Feature::Solid,
        Feature::HeaderEncryption,
        Feature::QuickOpen,
    ];

    /// Stable machine-readable name.
    pub const fn name(self) -> &'static str {
        match self {
            Feature::Solid => "solid",
            Feature::HeaderEncryption => "header_encryption",
            Feature::QuickOpen => "quick_open",
        }
    }
}

/// The set of features a writer invocation uses.
///
/// Mirrors the reference `FeatureSet`: store-only archives support none of
/// the optional capabilities.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FeatureSet {
    pub solid: bool,
    pub header_encryption: bool,
    pub quick_open: bool,
}

impl FeatureSet {
    /// The feature set of a plain stored archive (no optional features).
    pub const fn store_only() -> Self {
        FeatureSet {
            solid: false,
            header_encryption: false,
            quick_open: false,
        }
    }

    /// Whether `feature` is enabled in this set.
    pub const fn contains(self, feature: Feature) -> bool {
        match feature {
            Feature::Solid => self.solid,
            Feature::HeaderEncryption => self.header_encryption,
            Feature::QuickOpen => self.quick_open,
        }
    }
}

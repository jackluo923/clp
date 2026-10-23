//! Work spent selecting lossless column encodings, independently of the Zstd level.

/// Shared candidate-selection policy for integer and floating-point columns.
/// Every effort keeps a lossless fallback and chooses by final compressed size.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[cfg_attr(feature = "cli", derive(clap::ValueEnum))]
#[repr(u8)]
pub enum EncodingEffort {
    /// Keep common encodings and one packed-delta alternative; skip decimal float trials.
    Fast,
    /// Keep the established integer menu and screen the useful exact decimal float candidates.
    #[default]
    Balanced,
    /// Also try less likely representations, including sparse RLE and broader decimal conversions.
    Exhaustive,
}

impl std::fmt::Display for EncodingEffort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Fast => "fast",
            Self::Balanced => "balanced",
            Self::Exhaustive => "exhaustive",
        })
    }
}

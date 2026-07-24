//! Versioned, explicit model metadata for the supported Hy-MT2 family.
//!
//! A profile is selected explicitly from `[endpoint].profile`; `Generic` keeps
//! unknown models usable while withholding family-specific tokenizer guarantees.
//! Profile sampler guidance is for service deployment documentation, never
//! automatic request-payload injection.

use crate::config::{GenerationSettings, Setting};

/// The architecture family behind a supported Hy-MT2 model profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchitectureVariant {
    /// No architecture claim is made for an untested generic model.
    Unknown,
    /// The 1.8B dense Hunyuan variant.
    Dense1_8B,
    /// The 7B dense Hunyuan variant.
    Dense7B,
    /// The 30B total-parameter, 3B-active mixture-of-experts variant.
    MoE30BA3B,
}

impl ArchitectureVariant {
    /// Stable human-readable architecture label for diagnostics.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Dense1_8B => "1.8B dense",
            Self::Dense7B => "7B dense",
            Self::MoE30BA3B => "30B-A3B MoE",
        }
    }
}

/// Immutable upstream source identity for a model or tokenizer artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpstreamSource {
    /// Hugging Face model repository.
    pub repo: &'static str,
    /// Immutable upstream revision used for this profile.
    pub revision: &'static str,
}

/// Supported Hy-MT2 model profiles plus a deliberately untested generic mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelProfile {
    /// Tencent Hy-MT2 1.8B dense model.
    HyMt2_1_8b,
    /// Tencent Hy-MT2 7B dense model.
    HyMt2_7b,
    /// Tencent Hy-MT2 30B-A3B mixture-of-experts model.
    HyMt2_30bA3b,
    /// Unknown or untested model with no family-specific guarantees.
    Generic,
}

const HY_MT2_1_8B: UpstreamSource = UpstreamSource {
    repo: "tencent/Hy-MT2-1.8B",
    revision: "9a341cd1b679d3efd23b46e847b01745a71ed792",
};
const HY_MT2_7B: UpstreamSource = UpstreamSource {
    repo: "tencent/Hy-MT2-7B",
    revision: "9b0eb4e8f001def3e5ff6469a0ac96fdb39ec223",
};
const HY_MT2_30B_A3B: UpstreamSource = UpstreamSource {
    repo: "tencent/Hy-MT2-30B-A3B",
    revision: "d3ead4dba61c09aac60a261a96ad1df3e705febb",
};

impl ModelProfile {
    /// Parse an explicit stable profile identifier from configuration.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "hy_mt2_1_8b" => Some(Self::HyMt2_1_8b),
            "hy_mt2_7b" => Some(Self::HyMt2_7b),
            "hy_mt2_30b_a3b" => Some(Self::HyMt2_30bA3b),
            "generic" => Some(Self::Generic),
            _ => None,
        }
    }

    /// Stable identifier persisted in configuration and diagnostics.
    pub const fn id(self) -> &'static str {
        match self {
            Self::HyMt2_1_8b => "hy_mt2_1_8b",
            Self::HyMt2_7b => "hy_mt2_7b",
            Self::HyMt2_30bA3b => "hy_mt2_30b_a3b",
            Self::Generic => "generic",
        }
    }

    /// Upstream model source pinned for the profile, if it is a tested family member.
    pub const fn model(self) -> Option<&'static UpstreamSource> {
        match self {
            Self::HyMt2_1_8b => Some(&HY_MT2_1_8B),
            Self::HyMt2_7b => Some(&HY_MT2_7B),
            Self::HyMt2_30bA3b => Some(&HY_MT2_30B_A3B),
            Self::Generic => None,
        }
    }

    /// Exact tokenizer source pinned for the profile.
    ///
    /// Each Hy-MT2 variant has an independently pinned tokenizer identity. Do
    /// not substitute another variant's tokenizer without a compatibility test.
    pub const fn tokenizer(self) -> Option<&'static UpstreamSource> {
        self.model()
    }

    /// Architecture family, when known.
    pub const fn architecture(self) -> ArchitectureVariant {
        match self {
            Self::HyMt2_1_8b => ArchitectureVariant::Dense1_8B,
            Self::HyMt2_7b => ArchitectureVariant::Dense7B,
            Self::HyMt2_30bA3b => ArchitectureVariant::MoE30BA3B,
            Self::Generic => ArchitectureVariant::Unknown,
        }
    }

    /// Maximum context supported by the upstream model configuration.
    pub const fn max_context_tokens(self) -> u32 {
        match self {
            Self::HyMt2_1_8b | Self::HyMt2_7b | Self::HyMt2_30bA3b => 262_144,
            Self::Generic => 0,
        }
    }

    /// Upstream recommended maximum generation length.
    pub const fn recommended_max_output_tokens(self) -> u32 {
        match self {
            Self::HyMt2_1_8b | Self::HyMt2_7b | Self::HyMt2_30bA3b => 4_096,
            Self::Generic => 4_096,
        }
    }

    /// Known aliases used by compatible GGUF artifacts.
    pub const fn gguf_aliases(self) -> &'static [&'static str] {
        match self {
            Self::HyMt2_1_8b => &["hy-mt2-1.8b", "hy-mt2-1.8b-gguf"],
            Self::HyMt2_7b => &["hy-mt2-7b", "hy-mt2-7b-gguf"],
            Self::HyMt2_30bA3b => &["hy-mt2-30b-a3b", "hy-mt2-30b-a3b-fp8"],
            Self::Generic => &[],
        }
    }

    /// Semantic upstream sampling guidance for service deployment.
    ///
    /// These values document tested Hy-MT2 recommendations and are not merged
    /// into client requests. The inference service owns default sampling until
    /// an explicit `[inference.override]` value is configured.
    pub const fn generation_defaults(self) -> GenerationSettings {
        match self {
            Self::HyMt2_1_8b | Self::HyMt2_7b => GenerationSettings {
                temperature: Setting::Value(0.7),
                top_p: Setting::Value(0.6),
                top_k: Setting::Value(20),
                repetition_penalty: Setting::Value(1.05),
                min_p: Setting::ServerDefault,
                repeat_last_n: Setting::ServerDefault,
            },
            Self::HyMt2_30bA3b => GenerationSettings {
                temperature: Setting::Value(0.7),
                top_p: Setting::Value(1.0),
                top_k: Setting::Disabled,
                repetition_penalty: Setting::Value(1.0),
                min_p: Setting::ServerDefault,
                repeat_last_n: Setting::ServerDefault,
            },
            Self::Generic => GenerationSettings::server_defaults(),
        }
    }

    /// Whether this profile is deliberately operating without tested assumptions.
    pub const fn is_generic(self) -> bool {
        matches!(self, Self::Generic)
    }
}

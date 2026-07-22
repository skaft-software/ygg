//! Compatibility tiers derived from the central capability model.

use crate::capabilities::{ColorDepth, TerminalCapabilities};

/// Legacy progressive-enhancement tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CapabilityTier {
    Baseline = 0,
    TrueColor = 1,
    NerdFont = 2,
    KittyProtocol = 3,
    GpuTerminal = 4,
}

impl CapabilityTier {
    pub fn capabilities(self) -> TerminalCapabilities {
        match self {
            Self::Baseline => TerminalCapabilities::interactive(ColorDepth::Ansi16, false),
            Self::TrueColor => TerminalCapabilities::interactive(ColorDepth::TrueColor, true),
            Self::NerdFont => TerminalCapabilities::interactive(ColorDepth::TrueColor, true)
                .with_overrides(&crate::capabilities::CapabilityOverrides {
                    nerd_font: Some(true),
                    ..crate::capabilities::CapabilityOverrides::default()
                }),
            Self::KittyProtocol | Self::GpuTerminal => {
                TerminalCapabilities::interactive(ColorDepth::TrueColor, true).with_overrides(
                    &crate::capabilities::CapabilityOverrides {
                        synchronized_output: Some(true),
                        hyperlinks: Some(true),
                        kitty_graphics: Some(true),
                        kitty_keyboard: Some(true),
                        ..crate::capabilities::CapabilityOverrides::default()
                    },
                )
            }
        }
    }
}

/// Detect a compatibility tier from the central model.
pub fn detect_tier() -> CapabilityTier {
    let capabilities = TerminalCapabilities::detect();
    if capabilities.kitty_graphics && capabilities.synchronized_output {
        CapabilityTier::KittyProtocol
    } else if capabilities.nerd_font {
        CapabilityTier::NerdFont
    } else if capabilities.color_depth == ColorDepth::TrueColor {
        CapabilityTier::TrueColor
    } else {
        CapabilityTier::Baseline
    }
}

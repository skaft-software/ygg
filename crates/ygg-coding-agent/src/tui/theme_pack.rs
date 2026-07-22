#![allow(missing_docs)]

#[derive(Clone, Copy, Debug)]
pub(crate) struct BundledTheme {
    pub id: &'static str,
    pub source: &'static str,
}

pub(crate) const THEMES: &[BundledTheme] = &[
    BundledTheme {
        id: "bone-machine",
        source: include_str!("../../themes/bone-machine.toml"),
    },
    BundledTheme {
        id: "circuit-garden",
        source: include_str!("../../themes/circuit-garden.toml"),
    },
    BundledTheme {
        id: "field-notes",
        source: include_str!("../../themes/field-notes.toml"),
    },
    BundledTheme {
        id: "oxide-console",
        source: include_str!("../../themes/oxide-console.toml"),
    },
    BundledTheme {
        id: "paper-ledger",
        source: include_str!("../../themes/paper-ledger.toml"),
    },
    BundledTheme {
        id: "signal-noir",
        source: include_str!("../../themes/signal-noir.toml"),
    },
    BundledTheme {
        id: "synthwave-relay",
        source: include_str!("../../themes/synthwave-relay.toml"),
    },
    BundledTheme {
        id: "tidepool",
        source: include_str!("../../themes/tidepool.toml"),
    },
    BundledTheme {
        id: "violet-hour",
        source: include_str!("../../themes/violet-hour.toml"),
    },
    BundledTheme {
        id: "zen-mono",
        source: include_str!("../../themes/zen-mono.toml"),
    },
];

pub(crate) fn find(name: &str) -> Option<BundledTheme> {
    THEMES
        .iter()
        .copied()
        .find(|theme| theme.id.eq_ignore_ascii_case(name.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_has_ten_unique_stable_ids() {
        let mut ids = THEMES.iter().map(|theme| theme.id).collect::<Vec<_>>();
        assert_eq!(ids.len(), 10);
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 10);
        assert!(find("TIDEPOOL").is_some());
    }
}

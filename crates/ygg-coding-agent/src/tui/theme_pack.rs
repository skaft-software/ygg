#![allow(missing_docs)]

/// Renderer fixtures for named-theme regression tests. This module is compiled
/// only under `cfg(test)` and is not included in release binaries.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BundledTheme {
    pub id: &'static str,
    pub source: &'static str,
}

pub(crate) const THEMES: &[BundledTheme] = &[
    BundledTheme {
        id: "clawed",
        source: include_str!("../../themes/clawed.toml"),
    },
    BundledTheme {
        id: "kodex",
        source: include_str!("../../themes/kodex.toml"),
    },
    BundledTheme {
        id: "pie",
        source: include_str!("../../themes/pie.toml"),
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
    fn pack_has_three_unique_stable_ids() {
        let mut ids = THEMES.iter().map(|theme| theme.id).collect::<Vec<_>>();
        assert_eq!(ids.len(), 3);
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 3);
        assert!(find("CLAWED").is_some());
    }
}

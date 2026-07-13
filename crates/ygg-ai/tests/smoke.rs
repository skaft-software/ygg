//! Task 1.1 smoke test: the crate builds and the default compatibility mode is
//! `Strict` (design §5, §2 principle 7).

use ygg_ai::CompatibilityMode;

#[test]
fn default_compatibility_is_strict() {
    assert_eq!(CompatibilityMode::default(), CompatibilityMode::Strict);
}

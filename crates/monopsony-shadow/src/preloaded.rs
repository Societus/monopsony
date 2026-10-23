//! Preloaded shadow package recipes shipped with the distribution.
//!
//! These are catalog recipes available out of the box so a fresh install
//! can manage well-known third-party software without hand-writing TOML.
//! Every entry is covered by [`tests::preloaded_recipes_parse_and_validate`]
//! (parse + quick validation) so a malformed catalog recipe fails CI, not a
//! client at update time.
//!
//! Adding a recipe:
//! 1. Write `recipes/shadow/<name>.toml` at the repo root.
//! 2. Add an `include_str!` constant and register it in
//!    [`preloaded_recipe_tomls`].
//!
//! Notes on writing recipes for this catalog:
//! - `install.target_dir` may be `~/...` (per-user installs); it expands
//!   against the invoking user's home. Plain relative paths are rejected.
//! - `build.requires` entries are checked with `which`, so they must be
//!   executables on PATH (not distro package names).
//! - TOML literal strings (`'...'`) pass backslashes through verbatim:
//!   write regexes as `'^(\\d+)$'` in BASIC strings but `'^(\d+)$'` in
//!   LITERAL strings. Mixing these up silently breaks tag matching.

use crate::shadow_recipe::parse_recipe_str;
use monopsony_common::shadow_types::ShadowRecipe;

/// i2pd (PurpleI2P): git source build, user-mode install.
pub const PRELOADED_I2PD: &str = include_str!("../../../recipes/shadow/i2pd.toml");

/// All preloaded recipes as `(name, raw_toml)` pairs.
pub fn preloaded_recipe_tomls() -> Vec<(&'static str, &'static str)> {
    vec![("i2pd", PRELOADED_I2PD)]
}

/// Parse all preloaded recipes. Entries are CI-validated, so `filter_map`
/// only drops entries if a regression slips past the tests; callers that
/// need strictness can use [`preloaded_recipe_tomls`] and parse themselves.
pub fn preloaded_recipes() -> Vec<ShadowRecipe> {
    preloaded_recipe_tomls()
        .into_iter()
        .filter_map(|(_, toml)| parse_recipe_str(toml).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shadow_recipe::validate_recipe_quick;

    /// Load-bearing: every catalog recipe must parse as a `ShadowRecipe`
    /// and pass quick validation. Catches TOML syntax errors, schema
    /// drift, and invalid regexes at build time.
    #[test]
    fn preloaded_recipes_parse_and_validate() {
        for (name, toml) in preloaded_recipe_tomls() {
            let recipe = parse_recipe_str(toml)
                .unwrap_or_else(|e| panic!("preloaded recipe '{name}' failed to parse: {e}"));
            validate_recipe_quick(&recipe).unwrap_or_else(|errs| {
                panic!("preloaded recipe '{name}' failed validation: {errs:?}")
            });
        }
    }

    #[test]
    fn preloaded_recipe_names_are_unique() {
        let mut names: Vec<_> = preloaded_recipe_tomls()
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        names.sort_unstable();
        names.dedup();
        let all = preloaded_recipe_tomls().len();
        assert_eq!(names.len(), all, "duplicate preloaded recipe names");
    }

    #[test]
    fn i2pd_recipe_is_git_release_with_tilde_target() {
        let recipe = preloaded_recipes()
            .into_iter()
            .find(|r| r.package.name == "i2pd")
            .expect("i2pd recipe must be present in the catalog");
        assert!(matches!(
            recipe.package.source_type,
            monopsony_common::shadow_types::ShadowSource::GitRelease
        ));
        assert!(recipe.install.target_dir.starts_with("~/"));
        assert_eq!(
            recipe.install.method,
            monopsony_common::shadow_types::InstallMethod::Build
        );
    }
}

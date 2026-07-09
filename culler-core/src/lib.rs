//! culler-core: the pure, GUI-free domain library for FastCull.
//!
//! Modules land as phases progress: `model` + `persist` (phase 1),
//! then `scan`, `xmp`, `plan`, `apply`, `decode` in later phases.
//! Nothing in this crate depends on Slint or any GUI type.

pub mod model;
pub mod persist;
pub mod plan;
pub mod scan;
pub mod xmp;

#[cfg(test)]
mod tests {
    #[test]
    fn crate_builds() {
        assert_eq!(2 + 2, 4);
    }
}

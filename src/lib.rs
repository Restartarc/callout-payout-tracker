pub mod amm;
pub mod decode;
pub mod idl;
pub mod ledger;
pub mod payouts;
pub mod rows;

use std::path::PathBuf;

/// The IDL commit this build decodes with. Older history needs older commits.
pub const IDL_COMMIT: &str = "bc35796152027d2744ca02066d305438e34b67e7";

pub fn idl_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("idl").join(IDL_COMMIT)
}

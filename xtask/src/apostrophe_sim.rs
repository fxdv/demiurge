//! **'sim** — Demiurge fleet simulation spinoff entrypoint.

use std::error::Error;

pub fn apostrophe_sim() -> Result<(), Box<dyn Error>> {
    eprintln!("'sim: fleet simulation spinoff — trace replay + heterogeneous mock fleet");
    crate::load_bench::load_bench(false, None, false, false, true)?;
    crate::load_bench::load_report(false, false, true)?;
    Ok(())
}

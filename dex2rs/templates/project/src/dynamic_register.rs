//! Native registration; the CLI overwrites this file.
//!
//! Default (static) mode emits the dummy version below: methods are exported as
//! `Java_*` symbols and ART binds them by name, so there is nothing to do.

use dex2rs_runtime as d2c;

pub unsafe fn register(_env: d2c::Env) -> Result<(), String> {
    Ok(())
}

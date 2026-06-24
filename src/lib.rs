//! Generate a DuckDB extension that bridges a DataFission shim
//! into DuckDB as native functions / aggregates / types /
//! operators. See AGENTS.md for the target's quirks.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use shim_bridge_codegen_core::load_plan;

pub mod emit;

/// Generate a complete bridge crate from a shim-interface
/// SQLite database.
pub fn generate(interface_sqlite: &Path, out_dir: &Path) -> Result<()> {
    let plan = load_plan(interface_sqlite)
        .with_context(|| format!("loading {}", interface_sqlite.display()))?;
    fs::create_dir_all(out_dir)?;
    fs::create_dir_all(out_dir.join("src"))?;

    fs::write(out_dir.join("Cargo.toml"), emit::cargo_toml(&plan))?;
    fs::write(out_dir.join("src/lib.rs"), emit::lib_rs(&plan))?;
    fs::write(out_dir.join("src/registry.rs"), emit::registry_rs(&plan))?;
    fs::write(out_dir.join("src/scalars.rs"), emit::scalars_rs(&plan))?;
    fs::write(out_dir.join("src/aggregates.rs"), emit::aggregates_rs(&plan))?;
    fs::write(out_dir.join("src/table_functions.rs"), emit::table_functions_rs(&plan))?;
    fs::write(out_dir.join("src/window_functions.rs"), emit::window_functions_rs(&plan))?;
    fs::write(out_dir.join("src/types.rs"), emit::types_rs(&plan))?;
    fs::write(out_dir.join("src/operators.rs"), emit::operators_rs(&plan))?;
    fs::write(out_dir.join("src/casts.rs"), emit::casts_rs(&plan))?;
    fs::write(out_dir.join("src/preprocessors.rs"), emit::preprocessors_rs(&plan))?;
    fs::write(out_dir.join("src/system_catalog.rs"), emit::system_catalog_rs(&plan))?;
    fs::write(out_dir.join("src/spatial_indexes.rs"), emit::spatial_indexes_rs(&plan))?;
    fs::write(out_dir.join("README.md"), emit::readme(&plan))?;
    Ok(())
}

pub(crate) use shim_bridge_codegen_core as core;

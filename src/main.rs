//! CLI driver for ducklink-shim-codegen.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "ducklink-shim-codegen",
    about = "Generate a DuckDB extension crate bridging a DataFission shim into DuckDB."
)]
struct Args {
    /// Path to a shim-interface `.sqlite` (produced by
    /// `postgis-shim-interface` / `mobilitydb-shim-interface`).
    #[arg(long)]
    interface: PathBuf,

    /// Output directory for the generated bridge crate.
    /// Created if missing; existing files are overwritten.
    #[arg(long)]
    out: PathBuf,
}

fn main() -> Result<()> {
    let args = Args::parse();
    ducklink_shim_codegen::generate(&args.interface, &args.out)?;
    eprintln!("Wrote bridge crate to {}", args.out.display());
    Ok(())
}

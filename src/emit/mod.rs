//! Per-kind emit modules for the DuckDB bridge skeleton.

use crate::core::BridgePlan;

pub fn cargo_toml(plan: &BridgePlan) -> String {
    let crate_name = sanitize_crate_name(&primary_extension_name(plan));
    format!(
r##"[package]
name = "{name}-duckdb-bridge"
version = "0.1.0"
edition = "2021"
description = "Generated DuckDB extension that bridges the {name} DataFission shim into DuckDB."
license = "Apache-2.0"

[lib]
name = "{name}_duckdb_bridge"
crate-type = ["cdylib", "rlib"]

[dependencies]
# Path-deps into the source DataFission tree so we can call the
# loader's wasm scalar-invoke surface directly. Move to git-deps
# when DataFission ships releases.
datafission-df-plugin-loader = {{ path = "../datafission/crates/df-plugin-loader" }}
datafission-df-plugin-api    = {{ path = "../datafission/crates/df-plugin-api" }}
datafission-functions        = {{ path = "../datafission/crates/functions" }}

# The duckdb crate ships the `vscalar` (vectorised scalar
# function) trait + the `duckdb_entrypoint_c_api` macro that
# expands to the C-ABI loadable-extension entry point. We pin
# to the latest 1.x release.
duckdb                  = {{ version = "1", features = ["vscalar", "loadable-extension"] }}
duckdb-loadable-macros  = "1"
libduckdb-sys           = "1"

anyhow      = "1"
once_cell   = "1"
parking_lot = "0.12"
tracing     = "0.1"
serde_json  = "1"

[profile.release]
lto         = true
codegen-units = 1
opt-level   = "z"
strip       = true
"##,
        name = crate_name,
    )
}

pub fn lib_rs(plan: &BridgePlan) -> String {
    let mut s = generated_header();
    let crate_name = sanitize_crate_name(&primary_extension_name(plan));
    s.push_str(&format!(
r##"//! Generated DuckDB extension entry point.
//!
//! Load with:
//!   duckdb> INSTALL '{name}_duckdb_bridge' FROM '/path/to/dir';
//!   duckdb> LOAD '{name}_duckdb_bridge';
//!
//! Phase 1 (2026-06-24): scalar dispatch wired through
//! df-plugin-loader. ST_GeomFromText is fully functional; the
//! other categories (aggregates, UDTFs, window funcs, types,
//! operators, casts, preprocessors, system catalog, spatial
//! indexes) are scaffold-only — see per-module TODOs and
//! AGENTS.md for the phased plan.

pub mod registry;
pub mod scalars;
pub mod aggregates;
pub mod table_functions;
pub mod window_functions;
pub mod types;
pub mod operators;
pub mod casts;
pub mod preprocessors;
pub mod system_catalog;
pub mod spatial_indexes;

use std::error::Error;

use duckdb::{{Connection, Result}};
use duckdb_loadable_macros::duckdb_entrypoint_c_api;

/// DuckDB extension entry point. The `duckdb_entrypoint_c_api`
/// macro expands to the C-ABI `<name>_init_c_api` symbol that
/// DuckDB's `LOAD` looks up. The macro also injects the
/// version-compatibility check (we require DuckDB ≥ v0.0.1 —
/// any version that supports the vscalar API).
///
/// The composed shim wasm path comes from the
/// `{env}` env var so the bridge isn't pinned to
/// a build-time location (matches the host's runtime-loaded
/// model).
#[duckdb_entrypoint_c_api(ext_name = "{name}_duckdb_bridge", min_duckdb_version = "v0.0.1")]
pub fn extension_entrypoint(conn: Connection) -> Result<(), Box<dyn Error>> {{
    registry::load_shim().map_err(|e| {{
        // Use the alternate `:#` formatter so anyhow walks the
        // cause chain; without it, the inner wasm-loader error
        // gets hidden behind whatever with_context wrapper is
        // outermost.
        format!("shim load: {{e:#}}")
    }})?;
    scalars::register_all(&conn).map_err(|e| {{
        format!("scalar registration: {{e}}")
    }})?;
    Ok(())
}}
"##,
        name = crate_name,
        env = format!("{}_SHIM_WASM", primary_extension_name(plan).to_uppercase().replace('-', "_")),
    ));
    s.push_str(&format!(
        "// Extensions loaded by this bridge:\n//\n{}\n",
        plan.extensions
            .iter()
            .map(|e| format!(
                "//   - {} v{}  ({} scalars, {} agg, {} udtf, {} window, {} types, \
                 {} ops, {} casts, {} preps, {} catalog, {} indexes)",
                e.name, e.version,
                e.scalars.len(), e.aggregates.len(),
                e.table_functions.len(), e.window_functions.len(),
                e.column_types.len(), e.operators.len(),
                e.cast_rewrites.len(), e.preprocessor_patterns.len(),
                e.system_catalog_tables.len(), e.spatial_indexes.len()
            ))
            .collect::<Vec<_>>()
            .join("\n")
    ));
    s
}

pub fn scalars_rs(plan: &BridgePlan) -> String {
    let mut s = generated_header();
    s.push_str(
r##"//! Scalar-function registration.
//!
//! Phase 1 (2026-06-24): vectorised scalar dispatch wired up;
//! ST_GeomFromText is fully functional. Other scalars are
//! listed below as comments.
//!
//! Dispatch shape (DuckDB is VECTORISED — every invoke is
//! handed a chunk of N rows; SQLite was 1-at-a-time):
//!
//!   For each row in input chunk:
//!     - read varchar / blob / etc. into a FunctionValue
//!     - call shim's ScalarFunctionDef::execute(&[…])
//!     - write the result to the output FlatVector
//!
//! Phase 2 (2026-06-24) covers the top 7 PostGIS signature
//! shapes (about 70 % of the scalar surface):
//!
//!   text          → blob     ST_GeomFromText et al.       (~16)
//!   blob          → blob     ST_Centroid, ST_ConvexHull   (~71)
//!   blob,blob     → boolean  ST_Intersects, ST_Contains   (~33)
//!   blob          → f64      ST_Area, ST_Length, ST_X     (~32)
//!   blob,f64      → blob     ST_Buffer, ST_Simplify       (~23)
//!   blob          → text     ST_AsText, ST_AsGeoJSON      (~22)
//!   blob,blob     → blob     ST_Union, ST_Intersection    (~21)
//!
//! Each shape has its own VScalar marker struct and a
//! `register_<shape>` helper. The codegen detects each
//! scalar's shape and routes to the matching helper.

use std::sync::Arc;

use duckdb::{
    Connection, Result,
    core::{DataChunkHandle, Inserter, LogicalTypeHandle, LogicalTypeId},
    types::DuckString,
    vscalar::{ScalarFunctionSignature, VScalar},
    vtab::arrow::WritableVector,
};
use libduckdb_sys::duckdb_string_t;

use datafission_functions::traits::ScalarFunctionDef;
use datafission_functions::types::FunctionValue;

use crate::registry;

/// Register every scalar matching one of the Phase-2 signature
/// shapes against the given connection. Scalars whose shape is
/// not yet implemented are emitted as comments (with their
/// signature) so a future phase can wire them in.
pub fn register_all(conn: &Connection) -> Result<()> {
"##,
    );

    // Per-shape counters for the trailing comment + the
    // skipped-shapes histogram.
    let mut emitted: std::collections::HashMap<&'static str, usize> = Default::default();
    let mut skipped: std::collections::HashMap<String, usize> = Default::default();

    for ext in &plan.extensions {
        for sc in &ext.scalars {
            let shape = classify_shape(sc);
            match shape {
                Some(helper) => {
                    s.push_str(&format!(
                        "    {helper}(conn, \"{name}\")?;\n",
                        helper = helper, name = sc.canonical_name,
                    ));
                    for alias in &sc.aliases {
                        s.push_str(&format!(
                            "    {helper}(conn, \"{alias}\")?; // alias of {name}\n",
                            helper = helper, alias = alias, name = sc.canonical_name,
                        ));
                    }
                    *emitted.entry(helper).or_insert(0) += 1 + sc.aliases.len();
                }
                None => {
                    let v = sc.param_signatures.first()
                        .map(|v| v.iter().cloned().collect::<Vec<_>>().join(","))
                        .unwrap_or_else(|| "".into());
                    let key = format!("[{v}] -> {ret}", ret = sc.return_type);
                    *skipped.entry(key).or_insert(0) += 1;
                }
            }
        }
    }

    // Per-shape stats comment.
    let total_emitted: usize = emitted.values().sum();
    let total_skipped: usize = skipped.values().sum();
    s.push_str(&format!(
        "    // Phase 2: {total_emitted} names registered ({n_shapes} shapes), \
         {total_skipped} scalars deferred to Phase 3+ ({n_skipped} unique shapes).\n",
        n_shapes = emitted.len(),
        n_skipped = skipped.len(),
    ));

    s.push_str(PHASE2_RUNTIME);

    // Trailing histogram of deferred shapes — agents iterating
    // toward Phase 3 see exactly what's left.
    if !skipped.is_empty() {
        s.push_str("\n// Deferred shapes (Phase 3 target):\n");
        let mut rows: Vec<_> = skipped.iter().collect();
        rows.sort_by(|a, b| b.1.cmp(a.1));
        for (shape, count) in rows.iter().take(15) {
            s.push_str(&format!("//   {:>4}  {}\n", count, shape));
        }
    }
    s
}

/// Map a scalar's `param_signatures[0]` + return_type pair to a
/// Phase-2 shape helper name. Returns None when the shape isn't
/// covered (caller emits as a comment for Phase 3).
fn classify_shape(sc: &shim_bridge_codegen_core::ScalarFn) -> Option<&'static str> {
    let v = sc.param_signatures.first()?;
    let pat: Vec<&str> = v.iter().map(|s| s.as_str()).collect();
    let ret = sc.return_type.as_str();
    match (pat.as_slice(), ret) {
        (["text"], "binary")                 => Some("register_text_to_blob"),
        (["binary"], "binary")               => Some("register_blob_to_blob"),
        (["binary", "binary"], "boolean")    => Some("register_blob_blob_to_bool"),
        (["binary"], "float64")              => Some("register_blob_to_f64"),
        (["binary", "float64"], "binary")    => Some("register_blob_f64_to_blob"),
        (["binary"], "text")                 => Some("register_blob_to_text"),
        (["binary", "binary"], "binary")     => Some("register_blob_blob_to_blob"),
        (["binary", "binary"], "float64")    => Some("register_blob_blob_to_f64"),
        // Phase 3a (2026-06-24): six more shapes covering the
        // next ~55 PostGIS scalars.
        (["binary"], "uint32")               => Some("register_blob_to_u32"),
        (["binary"], "int32")                => Some("register_blob_to_i32"),
        (["binary", "uint32"], "binary")     => Some("register_blob_u32_to_blob"),
        (["binary", "int32"], "binary")      => Some("register_blob_i32_to_blob"),
        (["binary", "binary", "float64"], "boolean")
                                             => Some("register_blob_blob_f64_to_bool"),
        (["binary", "text"], "binary")       => Some("register_blob_text_to_blob"),
        _ => None,
    }
}

/// The Phase-2 runtime — all 7 shape helpers + their VScalar
/// impls. Emitted verbatim into every generated bridge so each
/// crate is self-contained.
const PHASE2_RUNTIME: &str = r##"    Ok(())
}

fn duckdb_sys_error(msg: String) -> duckdb::Error {
    // The duckdb crate doesn't expose a "userland error" variant
    // we can construct directly; round-trip through
    // ToSqlConversionFailure until they ship one.
    duckdb::Error::ToSqlConversionFailure(msg.into())
}

fn lookup(sql_name: &str) -> Result<Arc<dyn ScalarFunctionDef>> {
    registry::lookup_scalar(sql_name).ok_or_else(|| duckdb_sys_error(format!(
        "scalar `{sql_name}` not registered by the shim"
    )))
}

// ====================================================================
// Per-shape register_ + VScalar impls.
//
// Each shape has the same structure:
//   - register_<shape>(conn, name) looks up the ScalarFunctionDef
//     by name and registers it via register_scalar_function_with_state.
//   - <Shape>Scalar struct impls VScalar with State = Arc<…> and a
//     fixed (input types) → (output type) signature.
//   - invoke iterates rows in the DataChunk, reads each row's
//     args as FunctionValue, calls ScalarFunctionDef::execute,
//     writes the result to the output FlatVector.
//
// The varchar/blob read pattern uses duckdb_string_t + DuckString
// (handles both inline-short and pointer-long strings); the
// primitive (i64/f64/bool) reads use FlatVector::as_slice_with_len.
// ====================================================================

// ---- (varchar) -> blob ----
fn register_text_to_blob(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<TextToBlobScalar>(sql_name, &def)
}
struct TextToBlobScalar;
impl VScalar for TextToBlobScalar {
    type State = Arc<dyn ScalarFunctionDef>;
    fn invoke(
        state: &Self::State,
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let n = input.len();
        let v0 = input.flat_vector(0);
        let raw = unsafe { v0.as_slice_with_len::<duckdb_string_t>(n) };
        let mut out = output.flat_vector();
        for i in 0..n {
            let mut s_raw = raw[i];
            let s: String = DuckString::new(&mut s_raw).as_str().into_owned();
            let r = state.execute(&[FunctionValue::String(s)])
                .map_err(|e| format!("{e:?}"))?;
            insert_blob_result::<Self>(&mut out, i, r)?;
        }
        Ok(())
    }
    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![LogicalTypeHandle::from(LogicalTypeId::Varchar)],
            LogicalTypeHandle::from(LogicalTypeId::Blob),
        )]
    }
}

// ---- (blob) -> blob ----
fn register_blob_to_blob(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobToBlobScalar>(sql_name, &def)
}
struct BlobToBlobScalar;
impl VScalar for BlobToBlobScalar {
    type State = Arc<dyn ScalarFunctionDef>;
    fn invoke(
        state: &Self::State,
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let n = input.len();
        let v0 = input.flat_vector(0);
        let raw = unsafe { v0.as_slice_with_len::<duckdb_string_t>(n) };
        let mut out = output.flat_vector();
        for i in 0..n {
            let mut s_raw = raw[i];
            // BLOBs and VARCHAR share the duckdb_string_t shape;
            // DuckString returns the bytes via as_str (lossy UTF-8
            // decode) — fine for WKB since we round-trip through
            // the bytes layer.
            let bytes: Vec<u8> = DuckString::new(&mut s_raw)
                .as_bytes().to_vec();
            let r = state.execute(&[FunctionValue::Binary(bytes)])
                .map_err(|e| format!("{e:?}"))?;
            insert_blob_result::<Self>(&mut out, i, r)?;
        }
        Ok(())
    }
    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![LogicalTypeHandle::from(LogicalTypeId::Blob)],
            LogicalTypeHandle::from(LogicalTypeId::Blob),
        )]
    }
}

// ---- (blob, blob) -> boolean ----
fn register_blob_blob_to_bool(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobBlobToBoolScalar>(sql_name, &def)
}
struct BlobBlobToBoolScalar;
impl VScalar for BlobBlobToBoolScalar {
    type State = Arc<dyn ScalarFunctionDef>;
    fn invoke(
        state: &Self::State,
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let n = input.len();
        let v0 = input.flat_vector(0);
        let v1 = input.flat_vector(1);
        let r0 = unsafe { v0.as_slice_with_len::<duckdb_string_t>(n) };
        let r1 = unsafe { v1.as_slice_with_len::<duckdb_string_t>(n) };
        let mut out = output.flat_vector();
        // Same two-pass pattern as BlobToF64Scalar: can't hold the
        // mutable slice borrow AND call set_null in the same scope.
        let mut nulls: Vec<usize> = Vec::new();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<bool>(n) };
            for i in 0..n {
                let mut a = r0[i];
                let mut b = r1[i];
                let ab: Vec<u8> = DuckString::new(&mut a).as_bytes().to_vec();
                let bb: Vec<u8> = DuckString::new(&mut b).as_bytes().to_vec();
                let r = state.execute(&[
                    FunctionValue::Binary(ab),
                    FunctionValue::Binary(bb),
                ]).map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Boolean(b) => b,
                    FunctionValue::Int8(i)   => i != 0,
                    FunctionValue::Int32(i)  => i != 0,
                    FunctionValue::Int64(i)  => i != 0,
                    FunctionValue::Null      => { nulls.push(i); false },
                    other => return Err(format!(
                        "BlobBlobToBoolScalar: unexpected variant `{}`",
                        other.type_name()
                    ).into()),
                };
            }
        }
        for i in nulls { out.set_null(i); }
        Ok(())
    }
    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![
                LogicalTypeHandle::from(LogicalTypeId::Blob),
                LogicalTypeHandle::from(LogicalTypeId::Blob),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Boolean),
        )]
    }
}

// ---- (blob) -> f64 ----
fn register_blob_to_f64(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobToF64Scalar>(sql_name, &def)
}
struct BlobToF64Scalar;
impl VScalar for BlobToF64Scalar {
    type State = Arc<dyn ScalarFunctionDef>;
    fn invoke(
        state: &Self::State,
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let n = input.len();
        let v0 = input.flat_vector(0);
        let raw = unsafe { v0.as_slice_with_len::<duckdb_string_t>(n) };
        let mut out = output.flat_vector();
        // Collect nulls separately because as_mut_slice_with_len
        // and set_null both borrow `out` mutably — can't have both
        // alive at the same iteration. Two-pass keeps lifetimes
        // disjoint.
        let mut nulls: Vec<usize> = Vec::new();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<f64>(n) };
            for i in 0..n {
                let mut s_raw = raw[i];
                let bytes: Vec<u8> = DuckString::new(&mut s_raw)
                    .as_bytes().to_vec();
                let r = state.execute(&[FunctionValue::Binary(bytes)])
                    .map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Float64(f) => f,
                    FunctionValue::Float32(f) => f as f64,
                    FunctionValue::Int32(i)   => i as f64,
                    FunctionValue::Int64(i)   => i as f64,
                    FunctionValue::Null       => { nulls.push(i); 0.0 },
                    other => return Err(format!(
                        "BlobToF64Scalar: unexpected variant `{}`",
                        other.type_name()
                    ).into()),
                };
            }
        }
        for i in nulls { out.set_null(i); }
        Ok(())
    }
    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![LogicalTypeHandle::from(LogicalTypeId::Blob)],
            LogicalTypeHandle::from(LogicalTypeId::Double),
        )]
    }
}

// ---- (blob, f64) -> blob ----
fn register_blob_f64_to_blob(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobF64ToBlobScalar>(sql_name, &def)
}
struct BlobF64ToBlobScalar;
impl VScalar for BlobF64ToBlobScalar {
    type State = Arc<dyn ScalarFunctionDef>;
    fn invoke(
        state: &Self::State,
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let n = input.len();
        let v0 = input.flat_vector(0);
        let v1 = input.flat_vector(1);
        let r0 = unsafe { v0.as_slice_with_len::<duckdb_string_t>(n) };
        let r1 = unsafe { v1.as_slice_with_len::<f64>(n) };
        let mut out = output.flat_vector();
        for i in 0..n {
            let mut s_raw = r0[i];
            let bytes: Vec<u8> = DuckString::new(&mut s_raw)
                .as_bytes().to_vec();
            let r = state.execute(&[
                FunctionValue::Binary(bytes),
                FunctionValue::Float64(r1[i]),
            ]).map_err(|e| format!("{e:?}"))?;
            insert_blob_result::<Self>(&mut out, i, r)?;
        }
        Ok(())
    }
    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![
                LogicalTypeHandle::from(LogicalTypeId::Blob),
                LogicalTypeHandle::from(LogicalTypeId::Double),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Blob),
        )]
    }
}

// ---- (blob) -> text ----
fn register_blob_to_text(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobToTextScalar>(sql_name, &def)
}
struct BlobToTextScalar;
impl VScalar for BlobToTextScalar {
    type State = Arc<dyn ScalarFunctionDef>;
    fn invoke(
        state: &Self::State,
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let n = input.len();
        let v0 = input.flat_vector(0);
        let raw = unsafe { v0.as_slice_with_len::<duckdb_string_t>(n) };
        let mut out = output.flat_vector();
        for i in 0..n {
            let mut s_raw = raw[i];
            let bytes: Vec<u8> = DuckString::new(&mut s_raw)
                .as_bytes().to_vec();
            let r = state.execute(&[FunctionValue::Binary(bytes)])
                .map_err(|e| format!("{e:?}"))?;
            match r {
                FunctionValue::String(s) => out.insert(i, s.as_str()),
                FunctionValue::Binary(b) => out.insert(i, b.as_slice()),
                FunctionValue::Null      => out.set_null(i),
                other => return Err(format!(
                    "BlobToTextScalar: unexpected variant `{}`",
                    other.type_name()
                ).into()),
            }
        }
        Ok(())
    }
    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![LogicalTypeHandle::from(LogicalTypeId::Blob)],
            LogicalTypeHandle::from(LogicalTypeId::Varchar),
        )]
    }
}

// ---- (blob, blob) -> blob ----
fn register_blob_blob_to_blob(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobBlobToBlobScalar>(sql_name, &def)
}
struct BlobBlobToBlobScalar;
impl VScalar for BlobBlobToBlobScalar {
    type State = Arc<dyn ScalarFunctionDef>;
    fn invoke(
        state: &Self::State,
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let n = input.len();
        let v0 = input.flat_vector(0);
        let v1 = input.flat_vector(1);
        let r0 = unsafe { v0.as_slice_with_len::<duckdb_string_t>(n) };
        let r1 = unsafe { v1.as_slice_with_len::<duckdb_string_t>(n) };
        let mut out = output.flat_vector();
        for i in 0..n {
            let mut a = r0[i];
            let mut b = r1[i];
            let ab: Vec<u8> = DuckString::new(&mut a).as_bytes().to_vec();
            let bb: Vec<u8> = DuckString::new(&mut b).as_bytes().to_vec();
            let r = state.execute(&[
                FunctionValue::Binary(ab),
                FunctionValue::Binary(bb),
            ]).map_err(|e| format!("{e:?}"))?;
            insert_blob_result::<Self>(&mut out, i, r)?;
        }
        Ok(())
    }
    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![
                LogicalTypeHandle::from(LogicalTypeId::Blob),
                LogicalTypeHandle::from(LogicalTypeId::Blob),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Blob),
        )]
    }
}

// ---- (blob, blob) -> f64 ----
fn register_blob_blob_to_f64(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobBlobToF64Scalar>(sql_name, &def)
}
struct BlobBlobToF64Scalar;
impl VScalar for BlobBlobToF64Scalar {
    type State = Arc<dyn ScalarFunctionDef>;
    fn invoke(
        state: &Self::State,
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let n = input.len();
        let v0 = input.flat_vector(0);
        let v1 = input.flat_vector(1);
        let r0 = unsafe { v0.as_slice_with_len::<duckdb_string_t>(n) };
        let r1 = unsafe { v1.as_slice_with_len::<duckdb_string_t>(n) };
        let mut out = output.flat_vector();
        let mut nulls: Vec<usize> = Vec::new();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<f64>(n) };
            for i in 0..n {
                let mut a = r0[i];
                let mut b = r1[i];
                let ab: Vec<u8> = DuckString::new(&mut a).as_bytes().to_vec();
                let bb: Vec<u8> = DuckString::new(&mut b).as_bytes().to_vec();
                let r = state.execute(&[
                    FunctionValue::Binary(ab),
                    FunctionValue::Binary(bb),
                ]).map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Float64(f) => f,
                    FunctionValue::Float32(f) => f as f64,
                    FunctionValue::Int32(i)   => i as f64,
                    FunctionValue::Int64(i)   => i as f64,
                    FunctionValue::Null       => { nulls.push(i); 0.0 },
                    other => return Err(format!(
                        "BlobBlobToF64Scalar: unexpected variant `{}`",
                        other.type_name()
                    ).into()),
                };
            }
        }
        for i in nulls { out.set_null(i); }
        Ok(())
    }
    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![
                LogicalTypeHandle::from(LogicalTypeId::Blob),
                LogicalTypeHandle::from(LogicalTypeId::Blob),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Double),
        )]
    }
}

// ---- (blob) -> u32 ----
fn register_blob_to_u32(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobToU32Scalar>(sql_name, &def)
}
struct BlobToU32Scalar;
impl VScalar for BlobToU32Scalar {
    type State = Arc<dyn ScalarFunctionDef>;
    fn invoke(
        state: &Self::State,
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let n = input.len();
        let v0 = input.flat_vector(0);
        let raw = unsafe { v0.as_slice_with_len::<duckdb_string_t>(n) };
        let mut out = output.flat_vector();
        let mut nulls: Vec<usize> = Vec::new();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<u32>(n) };
            for i in 0..n {
                let mut s_raw = raw[i];
                let bytes: Vec<u8> = DuckString::new(&mut s_raw).as_bytes().to_vec();
                let r = state.execute(&[FunctionValue::Binary(bytes)])
                    .map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::UInt32(u) => u,
                    FunctionValue::UInt64(u) => u as u32,
                    FunctionValue::Int32(i)  => i as u32,
                    FunctionValue::Int64(i)  => i as u32,
                    FunctionValue::Null      => { nulls.push(i); 0 },
                    other => return Err(format!(
                        "BlobToU32Scalar: unexpected variant `{}`",
                        other.type_name()
                    ).into()),
                };
            }
        }
        for i in nulls { out.set_null(i); }
        Ok(())
    }
    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![LogicalTypeHandle::from(LogicalTypeId::Blob)],
            LogicalTypeHandle::from(LogicalTypeId::UInteger),
        )]
    }
}

// ---- (blob) -> i32 ----
fn register_blob_to_i32(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobToI32Scalar>(sql_name, &def)
}
struct BlobToI32Scalar;
impl VScalar for BlobToI32Scalar {
    type State = Arc<dyn ScalarFunctionDef>;
    fn invoke(
        state: &Self::State,
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let n = input.len();
        let v0 = input.flat_vector(0);
        let raw = unsafe { v0.as_slice_with_len::<duckdb_string_t>(n) };
        let mut out = output.flat_vector();
        let mut nulls: Vec<usize> = Vec::new();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<i32>(n) };
            for i in 0..n {
                let mut s_raw = raw[i];
                let bytes: Vec<u8> = DuckString::new(&mut s_raw).as_bytes().to_vec();
                let r = state.execute(&[FunctionValue::Binary(bytes)])
                    .map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Int32(v)  => v,
                    FunctionValue::Int64(v)  => v as i32,
                    FunctionValue::UInt32(v) => v as i32,
                    FunctionValue::Null      => { nulls.push(i); 0 },
                    other => return Err(format!(
                        "BlobToI32Scalar: unexpected variant `{}`",
                        other.type_name()
                    ).into()),
                };
            }
        }
        for i in nulls { out.set_null(i); }
        Ok(())
    }
    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![LogicalTypeHandle::from(LogicalTypeId::Blob)],
            LogicalTypeHandle::from(LogicalTypeId::Integer),
        )]
    }
}

// ---- (blob, u32) -> blob ----
fn register_blob_u32_to_blob(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobU32ToBlobScalar>(sql_name, &def)
}
struct BlobU32ToBlobScalar;
impl VScalar for BlobU32ToBlobScalar {
    type State = Arc<dyn ScalarFunctionDef>;
    fn invoke(
        state: &Self::State,
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let n = input.len();
        let v0 = input.flat_vector(0);
        let v1 = input.flat_vector(1);
        let r0 = unsafe { v0.as_slice_with_len::<duckdb_string_t>(n) };
        let r1 = unsafe { v1.as_slice_with_len::<u32>(n) };
        let mut out = output.flat_vector();
        for i in 0..n {
            let mut s_raw = r0[i];
            let bytes: Vec<u8> = DuckString::new(&mut s_raw).as_bytes().to_vec();
            let r = state.execute(&[
                FunctionValue::Binary(bytes),
                FunctionValue::UInt32(r1[i]),
            ]).map_err(|e| format!("{e:?}"))?;
            insert_blob_result::<Self>(&mut out, i, r)?;
        }
        Ok(())
    }
    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![
                LogicalTypeHandle::from(LogicalTypeId::Blob),
                LogicalTypeHandle::from(LogicalTypeId::UInteger),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Blob),
        )]
    }
}

// ---- (blob, i32) -> blob ----
fn register_blob_i32_to_blob(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobI32ToBlobScalar>(sql_name, &def)
}
struct BlobI32ToBlobScalar;
impl VScalar for BlobI32ToBlobScalar {
    type State = Arc<dyn ScalarFunctionDef>;
    fn invoke(
        state: &Self::State,
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let n = input.len();
        let v0 = input.flat_vector(0);
        let v1 = input.flat_vector(1);
        let r0 = unsafe { v0.as_slice_with_len::<duckdb_string_t>(n) };
        let r1 = unsafe { v1.as_slice_with_len::<i32>(n) };
        let mut out = output.flat_vector();
        for i in 0..n {
            let mut s_raw = r0[i];
            let bytes: Vec<u8> = DuckString::new(&mut s_raw).as_bytes().to_vec();
            let r = state.execute(&[
                FunctionValue::Binary(bytes),
                FunctionValue::Int32(r1[i]),
            ]).map_err(|e| format!("{e:?}"))?;
            insert_blob_result::<Self>(&mut out, i, r)?;
        }
        Ok(())
    }
    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![
                LogicalTypeHandle::from(LogicalTypeId::Blob),
                LogicalTypeHandle::from(LogicalTypeId::Integer),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Blob),
        )]
    }
}

// ---- (blob, blob, f64) -> bool ----
fn register_blob_blob_f64_to_bool(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobBlobF64ToBoolScalar>(sql_name, &def)
}
struct BlobBlobF64ToBoolScalar;
impl VScalar for BlobBlobF64ToBoolScalar {
    type State = Arc<dyn ScalarFunctionDef>;
    fn invoke(
        state: &Self::State,
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let n = input.len();
        let v0 = input.flat_vector(0);
        let v1 = input.flat_vector(1);
        let v2 = input.flat_vector(2);
        let r0 = unsafe { v0.as_slice_with_len::<duckdb_string_t>(n) };
        let r1 = unsafe { v1.as_slice_with_len::<duckdb_string_t>(n) };
        let r2 = unsafe { v2.as_slice_with_len::<f64>(n) };
        let mut out = output.flat_vector();
        let mut nulls: Vec<usize> = Vec::new();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<bool>(n) };
            for i in 0..n {
                let mut a = r0[i];
                let mut b = r1[i];
                let ab: Vec<u8> = DuckString::new(&mut a).as_bytes().to_vec();
                let bb: Vec<u8> = DuckString::new(&mut b).as_bytes().to_vec();
                let r = state.execute(&[
                    FunctionValue::Binary(ab),
                    FunctionValue::Binary(bb),
                    FunctionValue::Float64(r2[i]),
                ]).map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Boolean(b) => b,
                    FunctionValue::Int8(v)   => v != 0,
                    FunctionValue::Int32(v)  => v != 0,
                    FunctionValue::Int64(v)  => v != 0,
                    FunctionValue::Null      => { nulls.push(i); false },
                    other => return Err(format!(
                        "BlobBlobF64ToBoolScalar: unexpected variant `{}`",
                        other.type_name()
                    ).into()),
                };
            }
        }
        for i in nulls { out.set_null(i); }
        Ok(())
    }
    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![
                LogicalTypeHandle::from(LogicalTypeId::Blob),
                LogicalTypeHandle::from(LogicalTypeId::Blob),
                LogicalTypeHandle::from(LogicalTypeId::Double),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Boolean),
        )]
    }
}

// ---- (blob, text) -> blob ----
fn register_blob_text_to_blob(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobTextToBlobScalar>(sql_name, &def)
}
struct BlobTextToBlobScalar;
impl VScalar for BlobTextToBlobScalar {
    type State = Arc<dyn ScalarFunctionDef>;
    fn invoke(
        state: &Self::State,
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let n = input.len();
        let v0 = input.flat_vector(0);
        let v1 = input.flat_vector(1);
        let r0 = unsafe { v0.as_slice_with_len::<duckdb_string_t>(n) };
        let r1 = unsafe { v1.as_slice_with_len::<duckdb_string_t>(n) };
        let mut out = output.flat_vector();
        for i in 0..n {
            let mut a = r0[i];
            let mut b = r1[i];
            let bytes: Vec<u8> = DuckString::new(&mut a).as_bytes().to_vec();
            // Second arg is VARCHAR — read as String via as_str.
            let text: String = DuckString::new(&mut b).as_str().into_owned();
            let r = state.execute(&[
                FunctionValue::Binary(bytes),
                FunctionValue::String(text),
            ]).map_err(|e| format!("{e:?}"))?;
            insert_blob_result::<Self>(&mut out, i, r)?;
        }
        Ok(())
    }
    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![
                LogicalTypeHandle::from(LogicalTypeId::Blob),
                LogicalTypeHandle::from(LogicalTypeId::Varchar),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Blob),
        )]
    }
}

// ---- shared blob-output writer ----
fn insert_blob_result<S>(
    out: &mut duckdb::core::FlatVector<'_>,
    idx: usize,
    r: FunctionValue,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    match r {
        FunctionValue::Binary(b) => out.insert(idx, b.as_slice()),
        FunctionValue::String(s) => out.insert(idx, s.as_bytes()),
        FunctionValue::Null      => out.set_null(idx),
        other => return Err(format!(
            "{}: unexpected blob-output variant `{}`",
            std::any::type_name::<S>().split("::").last().unwrap_or("?"),
            other.type_name()
        ).into()),
    }
    Ok(())
}
"##;

pub fn registry_rs(plan: &BridgePlan) -> String {
    let mut s = generated_header();
    let env_var = format!(
        "{}_SHIM_WASM",
        primary_extension_name(plan).to_uppercase().replace('-', "_")
    );
    s.push_str(&format!(
r##"//! Shim registry — loads the composed wasm shim exactly once
//! at extension-init time and exposes a name → ScalarFunctionDef
//! lookup for the per-call dispatcher.
//!
//! Architecture is identical to sqlink's registry; the only
//! reason it lives here too is so the generated DuckDB bridge
//! is self-contained (no cross-bridge deps).

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{{Context, Result}};
use once_cell::sync::OnceCell;
use parking_lot::RwLock;

use datafission_df_plugin_api::{{
    DataTypePlugin, Extension, ExtensionError, ExtensionTarget, SystemCatalogProvider,
}};
use datafission_df_plugin_loader::RuntimeWasmExtension;
use datafission_functions::traits::{{
    AggregateFunctionDef, ScalarFunctionDef, TableFunctionDef, WindowFunctionDef,
}};

/// Lazily-loaded shim handle. Initialised in `load_shim()` from
/// the `{env}` env var.
static SHIM: OnceCell<ShimRegistry> = OnceCell::new();

struct ShimRegistry {{
    _ext: RuntimeWasmExtension,  // keep the wasm Store alive
    scalars: RwLock<HashMap<String, Arc<dyn ScalarFunctionDef>>>,
}}

pub fn load_shim() -> Result<()> {{
    if SHIM.get().is_some() {{
        return Ok(());
    }}
    let path = std::env::var("{env}")
        .with_context(|| format!(
            "Set {env}=/path/to/composed-shim.wasm before LOAD"
        ))?;
    let ext = RuntimeWasmExtension::from_file(&path)
        .with_context(|| format!("loading shim {{path}}"))?;

    let mut capture = CapturingTarget {{ scalars: Vec::new() }};
    ext.register(&mut capture)
        .map_err(|e| anyhow::anyhow!("shim register: {{e}}"))?;

    let mut scalars = HashMap::with_capacity(capture.scalars.len() * 2);
    for def in capture.scalars {{
        let canonical = def.name().to_string();
        for alias in def.aliases() {{
            scalars.insert(alias.to_string(), Arc::clone(&def));
        }}
        scalars.insert(canonical, def);
    }}

    SHIM.set(ShimRegistry {{
        _ext: ext,
        scalars: RwLock::new(scalars),
    }}).map_err(|_| anyhow::anyhow!("ShimRegistry already initialised"))?;

    Ok(())
}}

pub fn lookup_scalar(name: &str) -> Option<Arc<dyn ScalarFunctionDef>> {{
    let r = SHIM.get()?;
    r.scalars.read().get(name).cloned()
}}

/// Minimal ExtensionTarget that just collects every scalar the
/// shim registers. Other categories are no-ops until later phases.
struct CapturingTarget {{
    scalars: Vec<Arc<dyn ScalarFunctionDef>>,
}}

impl ExtensionTarget for CapturingTarget {{
    fn register_scalar_function(
        &mut self,
        _namespace: &str,
        def: Arc<dyn ScalarFunctionDef>,
    ) -> std::result::Result<(), ExtensionError> {{
        self.scalars.push(def);
        Ok(())
    }}
    fn register_aggregate_function(
        &mut self,
        _namespace: &str,
        _def: Arc<dyn AggregateFunctionDef>,
    ) -> std::result::Result<(), ExtensionError> {{ Ok(()) }}
    fn register_table_function(
        &mut self,
        _namespace: &str,
        _def: Arc<dyn TableFunctionDef>,
    ) -> std::result::Result<(), ExtensionError> {{ Ok(()) }}
    fn register_window_function(
        &mut self,
        _namespace: &str,
        _def: Arc<dyn WindowFunctionDef>,
    ) -> std::result::Result<(), ExtensionError> {{ Ok(()) }}
    fn register_data_type(
        &mut self,
        _plugin: Arc<dyn DataTypePlugin>,
    ) -> std::result::Result<(), ExtensionError> {{ Ok(()) }}
    fn register_system_catalog_provider(
        &mut self,
        _provider: Arc<dyn SystemCatalogProvider>,
    ) -> std::result::Result<(), ExtensionError> {{ Ok(()) }}
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {{ self }}
}}
"##,
        env = env_var,
    ));
    s
}

pub fn aggregates_rs(plan: &BridgePlan) -> String {
    let mut s = generated_header();
    s.push_str(
r##"//! Aggregate-function registration.
//!
//! DuckDB AggregateFunction needs: state_size, initialize,
//! update, combine, finalize, destructor. State holds the shim's
//! accumulator handle.

"##,
    );
    for ext in &plan.extensions {
        for agg in &ext.aggregates {
            s.push_str(&format!(
                "// aggregate `{}` (grouped={}, partial={}, accepts_config={})\n",
                agg.canonical_name,
                agg.supports_grouped,
                agg.supports_partial,
                agg.accepts_config,
            ));
        }
    }
    s.push_str("\n// TODO: emit register_aggregate_function calls.\n");
    s
}

pub fn table_functions_rs(plan: &BridgePlan) -> String {
    let mut s = generated_header();
    s.push_str(
r##"//! Table-function registration via duckdb::TableFunction.
//! Each shim UDTF gets bind / init / function callbacks.

"##,
    );
    for ext in &plan.extensions {
        for tf in &ext.table_functions {
            s.push_str(&format!("// udtf `{}`\n", tf.canonical_name));
        }
    }
    s.push_str("\n// TODO: emit register_table_function calls.\n");
    s
}

pub fn window_functions_rs(plan: &BridgePlan) -> String {
    let mut s = generated_header();
    s.push_str(
r##"//! Window-function registration.
//!
//! DuckDB exposes window functions as a special aggregate
//! variant with extra hooks for partition + frame state.

"##,
    );
    for ext in &plan.extensions {
        for w in &ext.window_functions {
            s.push_str(&format!("// window `{}`\n", w.canonical_name));
        }
    }
    s.push_str("\n// TODO: wire window-capable aggregates.\n");
    s
}

pub fn types_rs(plan: &BridgePlan) -> String {
    let mut s = generated_header();
    s.push_str(
r##"//! Custom column types.
//!
//! DuckDB DOES have first-class custom types via
//! `LogicalType::user(name)`. Register each shim column-type as
//! a USER type that internally aliases BLOB (or VARCHAR for
//! text-based shims like WKT).
//!
//!   let geom = LogicalType::user("GEOMETRY");
//!   con.register_type("GEOMETRY", geom)?;
//!
//! Casts to/from native types are emitted in casts.rs.

"##,
    );
    for ext in &plan.extensions {
        for ct in &ext.column_types {
            s.push_str(&format!(
                "// type_id={:5} name={:<24} size={:>4}  cast_from={:?}  cast_to={:?}\n",
                ct.type_id, ct.type_name, ct.storage_size, ct.cast_from, ct.cast_to
            ));
        }
    }
    s.push_str("\n// TODO: emit register_type calls.\n");
    s
}

pub fn operators_rs(plan: &BridgePlan) -> String {
    let mut s = generated_header();
    s.push_str(
r##"//! Operator handling.
//!
//! DuckDB supports binary operators on custom types if there's
//! a registered scalar function with the right signature, but
//! the operator-name → function mapping is a per-binding
//! detail. The clean path is: register each `op_<symbol>` as a
//! plain scalar (already done in scalars.rs); document for users
//! that they should call the function form, OR run their SQL
//! through ducklink's preprocessor which rewrites
//! `a && b` → `op_and(a, b)`.

"##,
    );
    for ext in &plan.extensions {
        for op in &ext.operators {
            s.push_str(&format!(
                "// `{}` (lhs={:?}, rhs={:?})  →  {}\n",
                op.symbol, op.lhs_type_id, op.rhs_type_id, op.function_name
            ));
        }
    }
    s.push_str("\n// TODO: build operator rewrite table for ducklink preprocessor.\n");
    s
}

pub fn casts_rs(plan: &BridgePlan) -> String {
    let mut s = generated_header();
    s.push_str(
r##"//! CAST(x AS T) — DuckDB has CAST hooks for USER types.
//!
//!   con.register_cast_function::<MyCast>(
//!     LogicalType::Varchar,                 // source
//!     LogicalType::user("GEOMETRY"),        // target
//!     CastFunction::new(cast_fn)
//!   )?;
//!
//! This is cleaner than SQLite — we don't need a preprocessor
//! for casts at all if `source_kind == "any"`. The other source
//! kinds (`stringliteral`, `geographycolumn`) still need
//! preprocessor help because DuckDB casts are type-driven, not
//! source-shape-driven.

"##,
    );
    for ext in &plan.extensions {
        for c in &ext.cast_rewrites {
            s.push_str(&format!(
                "// CAST(<{}> AS {}) → {} (hint: {})\n",
                c.source_kind, c.target_type, c.function_name, c.source_fn_hint
            ));
        }
    }
    s.push_str("\n// TODO: emit register_cast_function calls for `any`-kind;\n\
                 //       defer non-`any` kinds to ducklink preprocessor.\n");
    s
}

pub fn preprocessors_rs(plan: &BridgePlan) -> String {
    let mut s = generated_header();
    s.push_str(r##"//! Token-level preprocessor patterns (shim-advertised).

"##);
    for ext in &plan.extensions {
        for p in &ext.preprocessor_patterns {
            s.push_str(&format!("// token `{}` → {}\n", p.op_token, p.function_name));
        }
    }
    s.push_str("\n// TODO: feed into ducklink preprocessor.\n");
    s
}

pub fn system_catalog_rs(plan: &BridgePlan) -> String {
    let mut s = generated_header();
    s.push_str(
r##"//! System-catalog tables as DuckDB table functions with a
//! static schema (one TableFunction per virtual table).

"##,
    );
    for ext in &plan.extensions {
        for ct in &ext.system_catalog_tables {
            s.push_str(&format!("// catalog `{}.{}`\n", ct.catalog_name, ct.table_name));
            for col in &ct.columns {
                s.push_str(&format!(
                    "//   {} {} ({})\n",
                    col.name,
                    col.data_type,
                    if col.nullable { "nullable" } else { "not null" },
                ));
            }
        }
    }
    s.push_str("\n// TODO: emit one register_table_function per catalog entry.\n");
    s
}

pub fn spatial_indexes_rs(plan: &BridgePlan) -> String {
    let mut s = generated_header();
    s.push_str(
r##"//! Spatial index registration.
//!
//! DuckDB has built-in ART (B-tree-ish) indexes plus an
//! `IndexExtensionEntry` hook for extension-defined index
//! kinds (used by the spatial extension's R-tree). Map each
//! shim spatial-index `type_id` to a DuckDB index extension
//! entry; SQL `CREATE INDEX … USING <name>` resolves through
//! the catalog.
//!
//! Older DuckDB versions without `IndexExtensionEntry` fall
//! back to registering an index-aware UDTF that participates
//! in the optimizer's predicate pushdown via the bind
//! callback.

"##,
    );
    for ext in &plan.extensions {
        for ix in &ext.spatial_indexes {
            s.push_str(&format!("// index `{}` type_id={}\n", ix.name, ix.type_id));
        }
    }
    s.push_str(
        "\n// TODO: register IndexExtensionEntry per shim index,\n\
         //       or fall back to UDTF + pushdown.\n",
    );
    s
}

pub fn readme(plan: &BridgePlan) -> String {
    let mut s = String::new();
    s.push_str("# Generated DuckDB bridge\n\n");
    s.push_str("This crate was produced by `ducklink-shim-codegen`. Do not\n");
    s.push_str("edit by hand — regenerate from the source `.sqlite`.\n\n");
    s.push_str("## Extensions wrapped\n\n");
    s.push_str(
        "| Extension | Version | Scalars | Aggregates | UDTFs | Windows | Types | \
         Operators | Casts | Preprocessors | Catalog | Indexes |\n",
    );
    s.push_str("|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n");
    for e in &plan.extensions {
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            e.name, e.version,
            e.scalars.len(),
            e.aggregates.len(),
            e.table_functions.len(),
            e.window_functions.len(),
            e.column_types.len(),
            e.operators.len(),
            e.cast_rewrites.len(),
            e.preprocessor_patterns.len(),
            e.system_catalog_tables.len(),
            e.spatial_indexes.len(),
        ));
    }
    s
}

fn generated_header() -> String {
    "// === GENERATED by ducklink-shim-codegen — do not edit by hand ===\n\n".into()
}

fn sanitize_crate_name(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

fn primary_extension_name(plan: &BridgePlan) -> String {
    plan.extensions.first().map(|e| e.name.clone()).unwrap_or_else(|| "shim".into())
}

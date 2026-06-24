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

# Stock duckdb-rs from crates.io. We use it for the typed
# `Connection` + `VScalar` trait (scalar dispatch). The
# aggregate / custom-type registration paths go directly
# through `libduckdb-sys` raw FFI — duckdb-rs doesn't surface
# those APIs in its safe wrapper but the C ABI is fully
# accessible via libduckdb-sys (a re-export of libduckdb-sys's
# `loadable-extension` feature).
duckdb         = {{ version = "~1.10504", features = ["vscalar", "loadable-extension"] }}
libduckdb-sys  = {{ version = "~1.10504", features = ["loadable-extension"] }}

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
use std::ffi::CString;

use duckdb::Connection;
use libduckdb_sys as ffi;

/// DuckDB extension entry point. We hand-roll the C-ABI symbol
/// instead of using `#[duckdb_entrypoint_c_api]` so we can pull
/// the raw `duckdb_connection` out for the parts of the surface
/// (aggregates, custom types, casts) that duckdb-rs doesn't
/// wrap. The macro builds a `Connection` from
/// `duckdb_database` and discards everything else; we need the
/// raw pointer.
///
/// The composed shim wasm path comes from the
/// `{env}` env var so the bridge isn't pinned to
/// a build-time location (matches the host's runtime-loaded
/// model).
///
/// # Safety
///
/// Called by DuckDB on LOAD. `info` and `access` are valid for
/// the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn {name}_duckdb_bridge_init_c_api(
    info: ffi::duckdb_extension_info,
    access: *const ffi::duckdb_extension_access,
) -> bool {{
    match entrypoint_inner(info, access) {{
        Ok(v)  => v,
        Err(e) => {{
            if let Some(set_error_fn) = (*access).set_error {{
                if let Ok(cs) = CString::new(e.to_string()) {{
                    set_error_fn(info, cs.as_ptr());
                }}
            }}
            false
        }}
    }}
}}

unsafe fn entrypoint_inner(
    info: ffi::duckdb_extension_info,
    access: *const ffi::duckdb_extension_access,
) -> Result<bool, Box<dyn Error>> {{
    // Step 1 — install function-pointer trampolines for the C
    // API version we built against (or refuse to load if the
    // running DuckDB is older).
    let have_api = ffi::duckdb_rs_extension_api_init(info, access, "v1.2.0")
        .map_err(|e| -> Box<dyn Error> {{ e.into() }})?;
    if !have_api {{
        return Ok(false);
    }}

    // Step 2 — get the database handle DuckDB just opened.
    let get_database = (*access).get_database
        .ok_or("get_database function pointer is null in duckdb_extension_access")?;
    let db_ptr = get_database(info);
    if db_ptr.is_null() {{
        return Ok(false);
    }}
    let db: ffi::duckdb_database = *db_ptr;

    // Step 3 — open a raw connection. We use this connection for
    // every registration call so types/aggregates/scalars all
    // land in the same catalog session. Leaked deliberately:
    // disconnecting here would invalidate registrations that
    // DuckDB holds by reference to this connection.
    let mut raw_con: ffi::duckdb_connection = std::ptr::null_mut();
    if ffi::duckdb_connect(db, &mut raw_con) != ffi::DuckDBSuccess {{
        return Err("duckdb_connect failed".into());
    }}

    // Step 4 — register custom types first (independent of the
    // shim; aliases-as-BLOB only need a duckdb_connection).
    // `CREATE TABLE t (g GEOMETRY)` works even if the shim
    // failed to load.
    types::register_all(raw_con);

    // Step 4b — register identity casts BLOB <-> alias so
    // INSERT INTO t (GEOMETRY) VALUES (ST_GeomFromText(...))
    // doesn't fail with "Unimplemented type for cast
    // (BLOB -> GEOMETRY)". Must run after types::register_all.
    casts::register_all(raw_con);

    // Step 5 — load the wasm shim.
    registry::load_shim()
        .map_err(|e| -> Box<dyn Error> {{ format!("shim load: {{e:#}}").into() }})?;

    // Step 6 — scalars route through duckdb-rs's `VScalar`
    // trait, which needs a `Connection`. Wrap the raw connection
    // we already opened. Connection::open_from_raw opens
    // another connection internally; we accept that — both go
    // to the same database, registrations are catalog-scoped.
    let conn = Connection::open_from_raw(db.cast())
        .map_err(|e| -> Box<dyn Error> {{ format!("open_from_raw: {{e}}").into() }})?;
    scalars::register_all(&conn)
        .map_err(|e| -> Box<dyn Error> {{ format!("scalar registration: {{e}}").into() }})?;

    // Step 7 — aggregates via raw libduckdb-sys.
    aggregates::register_all(raw_con);

    // Step 8 — UDTFs still go through duckdb-rs (vtab trait).
    table_functions::register_all(&conn)
        .map_err(|e| -> Box<dyn Error> {{ format!("table function registration: {{e}}").into() }})?;

    Ok(true)
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
                        "    if let Err(e) = {helper}(conn, \"{name}\") {{ \
                         eprintln!(\"[shim-scalars] skipping `{name}`: {{e}}\"); }}\n",
                        helper = helper, name = sc.canonical_name,
                    ));
                    for alias in &sc.aliases {
                        s.push_str(&format!(
                            "    if let Err(e) = {helper}(conn, \"{alias}\") {{ \
                             eprintln!(\"[shim-scalars] skipping `{alias}`: {{e}}\"); }} \
                             // alias of {name}\n",
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
        // Phase 4a (2026-06-24): six more long-tail shapes
        // covering the next ~42 PostGIS scalars (→ ~84% surface).
        (["binary"], "boolean")              => Some("register_blob_to_bool"),
        (["binary", "float64", "float64"], "binary")
                                             => Some("register_blob_f64_f64_to_blob"),
        (["binary", "uint32"], "text")       => Some("register_blob_u32_to_text"),
        (["binary", "uint32", "uint32"], "binary")
                                             => Some("register_blob_u32_u32_to_blob"),
        (["binary", "float64", "float64", "float64", "float64"], "binary")
                                             => Some("register_blob_4f64_to_blob"),
        (["text"], "text")                   => Some("register_text_to_text"),
        // Phase 4e (2026-06-24): top-9 mobilitydb-driven shapes.
        // These cover ~270 mobilitydb scalars (most temporal
        // accessors that return Int64 timestamps + most
        // (sequence, scalar) restriction ops) plus the stbox /
        // tbox constructors.
        (["binary"], "int64")                => Some("register_blob_to_i64"),
        (["binary"], "uint64")               => Some("register_blob_to_u64"),
        (["binary", "int64"], "binary")      => Some("register_blob_i64_to_blob"),
        (["binary", "int64"], "boolean")     => Some("register_blob_i64_to_bool"),
        (["binary", "int64"], "float64")     => Some("register_blob_i64_to_f64"),
        (["binary", "float64"], "boolean")   => Some("register_blob_f64_to_bool"),
        (["binary", "binary"], "int64")      => Some("register_blob_blob_to_i64"),
        (["binary", "int64", "int64"], "binary")
                                             => Some("register_blob_i64_i64_to_blob"),
        (["float64", "float64", "float64", "float64", "int64", "int64"], "binary")
                                             => Some("register_4f64_2i64_to_blob"),
        // Phase 4f (2026-06-24): last-7 long-tail mobilitydb
        // shapes. Each covers fewer functions than Phase 4e,
        // but together they get the bridge from ~90% to ~95%
        // coverage on the 1548-scalar mobilitydb surface.
        (["binary", "float64", "float64"], "boolean")
                                             => Some("register_blob_f64_f64_to_bool"),
        (["binary", "int64", "int64"], "int32")
                                             => Some("register_blob_i64_i64_to_i32"),
        (["binary", "float64", "int64"], "int32")
                                             => Some("register_blob_f64_i64_to_i32"),
        (["int64", "int64", "boolean", "boolean"], "binary")
                                             => Some("register_2i64_2bool_to_blob"),
        (["int32", "int32", "boolean", "boolean"], "binary")
                                             => Some("register_2i32_2bool_to_blob"),
        (["binary", "uint32"], "int64")      => Some("register_blob_u32_to_i64"),
        (["binary", "binary", "uint32"], "binary")
                                             => Some("register_blob_blob_u32_to_blob"),
        // Phase 4g (2026-06-24): ttext-driven shapes. Each
        // covers ≤3 mobilitydb scalars but they're all in the
        // critical-path for ttext / ttext-style accessor
        // surface (ttext_ever_eq / ttext_value_at_string /
        // ttext_substring etc.).
        (["binary", "text"], "boolean")      => Some("register_blob_text_to_bool"),
        (["binary", "binary"], "text")       => Some("register_blob_blob_to_text"),
        (["binary", "int64"], "text")        => Some("register_blob_i64_to_text"),
        (["binary", "text", "text"], "binary")
                                             => Some("register_blob_text_text_to_blob"),
        (["text", "binary"], "binary")       => Some("register_text_blob_to_blob"),
        // Phase 4h (2026-06-24): complete coverage — all remaining 35 shapes.
        (["binary", "binary"], "uint32") => Some("register_blob_blob_to_u32"),
        (["binary", "binary", "float64"], "float64") => Some("register_blob_blob_f64_to_f64"),
        (["binary", "binary", "float64", "float64"], "float64") => Some("register_blob_blob_f64_f64_to_f64"),
        (["binary", "binary", "float64", "uint32"], "float64") => Some("register_blob_blob_f64_u32_to_f64"),
        (["binary", "binary", "int64"], "float64") => Some("register_blob_blob_i64_to_f64"),
        (["binary", "boolean"], "binary") => Some("register_blob_bool_to_blob"),
        (["binary", "float64"], "int32") => Some("register_blob_f64_to_i32"),
        (["binary", "float64", "float64"], "float64") => Some("register_blob_f64_f64_to_f64"),
        (["binary", "float64", "float64", "float64"], "binary") => Some("register_blob_f64_f64_f64_to_blob"),
        (["binary", "float64", "float64", "float64"], "boolean") => Some("register_blob_f64_f64_f64_to_bool"),
        (["binary", "float64", "float64", "float64", "int64"], "float64") => Some("register_blob_f64_f64_f64_i64_to_f64"),
        (["binary", "float64", "float64", "int64"], "binary") => Some("register_blob_f64_f64_i64_to_blob"),
        (["binary", "float64", "float64", "int64"], "boolean") => Some("register_blob_f64_f64_i64_to_bool"),
        (["binary", "float64", "int64"], "binary") => Some("register_blob_f64_i64_to_blob"),
        (["binary", "float64", "int64"], "int64") => Some("register_blob_f64_i64_to_i64"),
        (["binary", "float64", "uint32"], "int32") => Some("register_blob_f64_u32_to_i32"),
        (["binary", "int32"], "boolean") => Some("register_blob_i32_to_bool"),
        (["binary", "int64"], "int32") => Some("register_blob_i64_to_i32"),
        (["binary", "int64"], "int64") => Some("register_blob_i64_to_i64"),
        (["binary", "int64", "int64"], "boolean") => Some("register_blob_i64_i64_to_bool"),
        (["binary", "int64", "int64"], "float64") => Some("register_blob_i64_i64_to_f64"),
        (["binary", "int64", "int64"], "int64") => Some("register_blob_i64_i64_to_i64"),
        (["binary", "int64", "int64"], "text") => Some("register_blob_i64_i64_to_text"),
        (["binary", "uint32"], "float64") => Some("register_blob_u32_to_f64"),
        (["binary", "uint32"], "int32") => Some("register_blob_u32_to_i32"),
        (["binary", "uint32", "float64"], "float64") => Some("register_blob_u32_f64_to_f64"),
        (["float64", "float64"], "float64") => Some("register_f64_f64_to_f64"),
        (["float64", "float64", "boolean", "boolean"], "binary") => Some("register_f64_f64_bool_bool_to_blob"),
        (["float64", "float64", "float64", "float64"], "binary") => Some("register_f64_f64_f64_f64_to_blob"),
        (["float64", "float64", "float64", "float64"], "float64") => Some("register_f64_f64_f64_f64_to_f64"),
        (["int64", "float64"], "binary") => Some("register_i64_f64_to_blob"),
        (["int64", "float64", "float64"], "binary") => Some("register_i64_f64_f64_to_blob"),
        (["text", "int64", "int64"], "binary") => Some("register_text_i64_i64_to_blob"),
        (["text", "int64", "int64"], "float64") => Some("register_text_i64_i64_to_f64"),
        (["uint64"], "int64") => Some("register_u64_to_i64"),
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

// Phase 3b (2026-06-24): NULL propagation.
//
// Per-shape `invoke` impls call `state.propagates_null()` once
// at the top of the loop and, for each row, check the held
// `FlatVector::row_is_null(row as u64)` on each input. We can't
// share a `row_has_null_input(input, row, n)` helper because
// the existing per-row code already holds the FlatVectors
// (which borrow `input`) — the helper would re-borrow `input`
// and trigger the borrow checker. Each shape inlines the check
// using its already-held v0 (and v1, v2 where applicable).

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
        let propagates_null = state.propagates_null();
        for i in 0..n {
            if propagates_null && v0.row_is_null(i as u64) {
                out.set_null(i);
                continue;
            }
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
        let propagates_null = state.propagates_null();
        for i in 0..n {
            if propagates_null && v0.row_is_null(i as u64) {
                out.set_null(i);
                continue;
            }
            let mut s_raw = raw[i];
            // BLOBs and VARCHAR share the duckdb_string_t shape;
            // DuckString::as_bytes returns the raw bytes (no UTF-8
            // conversion) — required for WKB round-tripping.
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
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<bool>(n) };
            for i in 0..n {
                if propagates_null
                    && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64))
                {
                    nulls.push(i);
                    continue;
                }
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
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<f64>(n) };
            for i in 0..n {
                if propagates_null && v0.row_is_null(i as u64) {
                    nulls.push(i);
                    continue;
                }
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
        let propagates_null = state.propagates_null();
        for i in 0..n {
            if propagates_null
                && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64))
            {
                out.set_null(i);
                continue;
            }
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
        let propagates_null = state.propagates_null();
        for i in 0..n {
            if propagates_null && v0.row_is_null(i as u64) {
                out.set_null(i);
                continue;
            }
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
        let propagates_null = state.propagates_null();
        for i in 0..n {
            if propagates_null
                && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64))
            {
                out.set_null(i);
                continue;
            }
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
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<f64>(n) };
            for i in 0..n {
                if propagates_null
                    && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64))
                {
                    nulls.push(i);
                    continue;
                }
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
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<u32>(n) };
            for i in 0..n {
                if propagates_null && v0.row_is_null(i as u64) {
                    nulls.push(i);
                    continue;
                }
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
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<i32>(n) };
            for i in 0..n {
                if propagates_null && v0.row_is_null(i as u64) {
                    nulls.push(i);
                    continue;
                }
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
        let propagates_null = state.propagates_null();
        for i in 0..n {
            if propagates_null
                && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64))
            {
                out.set_null(i);
                continue;
            }
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
        let propagates_null = state.propagates_null();
        for i in 0..n {
            if propagates_null
                && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64))
            {
                out.set_null(i);
                continue;
            }
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
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<bool>(n) };
            for i in 0..n {
                if propagates_null
                    && (v0.row_is_null(i as u64)
                        || v1.row_is_null(i as u64)
                        || v2.row_is_null(i as u64))
                {
                    nulls.push(i);
                    continue;
                }
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
        let propagates_null = state.propagates_null();
        for i in 0..n {
            if propagates_null
                && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64))
            {
                out.set_null(i);
                continue;
            }
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

// ---- (blob) -> bool ----
fn register_blob_to_bool(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobToBoolScalar>(sql_name, &def)
}
struct BlobToBoolScalar;
impl VScalar for BlobToBoolScalar {
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
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<bool>(n) };
            for i in 0..n {
                if propagates_null && v0.row_is_null(i as u64) {
                    nulls.push(i);
                    continue;
                }
                let mut s_raw = raw[i];
                let bytes: Vec<u8> = DuckString::new(&mut s_raw).as_bytes().to_vec();
                let r = state.execute(&[FunctionValue::Binary(bytes)])
                    .map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Boolean(b) => b,
                    FunctionValue::Int8(v)   => v != 0,
                    FunctionValue::Int32(v)  => v != 0,
                    FunctionValue::Int64(v)  => v != 0,
                    FunctionValue::Null      => { nulls.push(i); false },
                    other => return Err(format!(
                        "BlobToBoolScalar: unexpected variant `{}`",
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
            LogicalTypeHandle::from(LogicalTypeId::Boolean),
        )]
    }
}

// ---- (blob, f64, f64) -> blob ----
fn register_blob_f64_f64_to_blob(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobF64F64ToBlobScalar>(sql_name, &def)
}
struct BlobF64F64ToBlobScalar;
impl VScalar for BlobF64F64ToBlobScalar {
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
        let r1 = unsafe { v1.as_slice_with_len::<f64>(n) };
        let r2 = unsafe { v2.as_slice_with_len::<f64>(n) };
        let mut out = output.flat_vector();
        let propagates_null = state.propagates_null();
        for i in 0..n {
            if propagates_null
                && (v0.row_is_null(i as u64)
                    || v1.row_is_null(i as u64)
                    || v2.row_is_null(i as u64))
            {
                out.set_null(i);
                continue;
            }
            let mut s_raw = r0[i];
            let bytes: Vec<u8> = DuckString::new(&mut s_raw).as_bytes().to_vec();
            let r = state.execute(&[
                FunctionValue::Binary(bytes),
                FunctionValue::Float64(r1[i]),
                FunctionValue::Float64(r2[i]),
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
                LogicalTypeHandle::from(LogicalTypeId::Double),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Blob),
        )]
    }
}

// ---- (blob, u32) -> text ----
fn register_blob_u32_to_text(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobU32ToTextScalar>(sql_name, &def)
}
struct BlobU32ToTextScalar;
impl VScalar for BlobU32ToTextScalar {
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
        let propagates_null = state.propagates_null();
        for i in 0..n {
            if propagates_null
                && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64))
            {
                out.set_null(i);
                continue;
            }
            let mut s_raw = r0[i];
            let bytes: Vec<u8> = DuckString::new(&mut s_raw).as_bytes().to_vec();
            let r = state.execute(&[
                FunctionValue::Binary(bytes),
                FunctionValue::UInt32(r1[i]),
            ]).map_err(|e| format!("{e:?}"))?;
            match r {
                FunctionValue::String(s) => out.insert(i, s.as_str()),
                FunctionValue::Binary(b) => out.insert(i, b.as_slice()),
                FunctionValue::Null      => out.set_null(i),
                other => return Err(format!(
                    "BlobU32ToTextScalar: unexpected variant `{}`",
                    other.type_name()
                ).into()),
            }
        }
        Ok(())
    }
    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![
                LogicalTypeHandle::from(LogicalTypeId::Blob),
                LogicalTypeHandle::from(LogicalTypeId::UInteger),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Varchar),
        )]
    }
}

// ---- (blob, u32, u32) -> blob ----
fn register_blob_u32_u32_to_blob(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobU32U32ToBlobScalar>(sql_name, &def)
}
struct BlobU32U32ToBlobScalar;
impl VScalar for BlobU32U32ToBlobScalar {
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
        let r1 = unsafe { v1.as_slice_with_len::<u32>(n) };
        let r2 = unsafe { v2.as_slice_with_len::<u32>(n) };
        let mut out = output.flat_vector();
        let propagates_null = state.propagates_null();
        for i in 0..n {
            if propagates_null
                && (v0.row_is_null(i as u64)
                    || v1.row_is_null(i as u64)
                    || v2.row_is_null(i as u64))
            {
                out.set_null(i);
                continue;
            }
            let mut s_raw = r0[i];
            let bytes: Vec<u8> = DuckString::new(&mut s_raw).as_bytes().to_vec();
            let r = state.execute(&[
                FunctionValue::Binary(bytes),
                FunctionValue::UInt32(r1[i]),
                FunctionValue::UInt32(r2[i]),
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
                LogicalTypeHandle::from(LogicalTypeId::UInteger),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Blob),
        )]
    }
}

// ---- (blob, f64, f64, f64, f64) -> blob ----  e.g. ST_MakeEnvelope
fn register_blob_4f64_to_blob(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<Blob4F64ToBlobScalar>(sql_name, &def)
}
struct Blob4F64ToBlobScalar;
impl VScalar for Blob4F64ToBlobScalar {
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
        let v3 = input.flat_vector(3);
        let v4 = input.flat_vector(4);
        let r0 = unsafe { v0.as_slice_with_len::<duckdb_string_t>(n) };
        let r1 = unsafe { v1.as_slice_with_len::<f64>(n) };
        let r2 = unsafe { v2.as_slice_with_len::<f64>(n) };
        let r3 = unsafe { v3.as_slice_with_len::<f64>(n) };
        let r4 = unsafe { v4.as_slice_with_len::<f64>(n) };
        let mut out = output.flat_vector();
        let propagates_null = state.propagates_null();
        for i in 0..n {
            if propagates_null
                && (v0.row_is_null(i as u64)
                    || v1.row_is_null(i as u64)
                    || v2.row_is_null(i as u64)
                    || v3.row_is_null(i as u64)
                    || v4.row_is_null(i as u64))
            {
                out.set_null(i);
                continue;
            }
            let mut s_raw = r0[i];
            let bytes: Vec<u8> = DuckString::new(&mut s_raw).as_bytes().to_vec();
            let r = state.execute(&[
                FunctionValue::Binary(bytes),
                FunctionValue::Float64(r1[i]),
                FunctionValue::Float64(r2[i]),
                FunctionValue::Float64(r3[i]),
                FunctionValue::Float64(r4[i]),
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
                LogicalTypeHandle::from(LogicalTypeId::Double),
                LogicalTypeHandle::from(LogicalTypeId::Double),
                LogicalTypeHandle::from(LogicalTypeId::Double),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Blob),
        )]
    }
}

// ---- (text) -> text ----
fn register_text_to_text(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<TextToTextScalar>(sql_name, &def)
}
struct TextToTextScalar;
impl VScalar for TextToTextScalar {
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
        let propagates_null = state.propagates_null();
        for i in 0..n {
            if propagates_null && v0.row_is_null(i as u64) {
                out.set_null(i);
                continue;
            }
            let mut s_raw = raw[i];
            let s: String = DuckString::new(&mut s_raw).as_str().into_owned();
            let r = state.execute(&[FunctionValue::String(s)])
                .map_err(|e| format!("{e:?}"))?;
            match r {
                FunctionValue::String(s) => out.insert(i, s.as_str()),
                FunctionValue::Binary(b) => out.insert(i, b.as_slice()),
                FunctionValue::Null      => out.set_null(i),
                other => return Err(format!(
                    "TextToTextScalar: unexpected variant `{}`",
                    other.type_name()
                ).into()),
            }
        }
        Ok(())
    }
    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![LogicalTypeHandle::from(LogicalTypeId::Varchar)],
            LogicalTypeHandle::from(LogicalTypeId::Varchar),
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

// ---- Phase 4e helpers — mobilitydb-driven shapes ----

// ---- (blob) -> i64 ----
fn register_blob_to_i64(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobToI64Scalar>(sql_name, &def)
}
struct BlobToI64Scalar;
impl VScalar for BlobToI64Scalar {
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
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<i64>(n) };
            for i in 0..n {
                if propagates_null && v0.row_is_null(i as u64) {
                    nulls.push(i); continue;
                }
                let mut s_raw = raw[i];
                let bytes: Vec<u8> = DuckString::new(&mut s_raw).as_bytes().to_vec();
                let r = state.execute(&[FunctionValue::Binary(bytes)])
                    .map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Int64(v)  => v,
                    FunctionValue::Int32(v)  => v as i64,
                    FunctionValue::UInt64(v) => v as i64,
                    FunctionValue::UInt32(v) => v as i64,
                    FunctionValue::Null      => { nulls.push(i); 0 },
                    other => return Err(format!(
                        "BlobToI64Scalar: unexpected variant `{}`",
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
            LogicalTypeHandle::from(LogicalTypeId::Bigint),
        )]
    }
}

// ---- (blob) -> u64 ----
fn register_blob_to_u64(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobToU64Scalar>(sql_name, &def)
}
struct BlobToU64Scalar;
impl VScalar for BlobToU64Scalar {
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
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<u64>(n) };
            for i in 0..n {
                if propagates_null && v0.row_is_null(i as u64) {
                    nulls.push(i); continue;
                }
                let mut s_raw = raw[i];
                let bytes: Vec<u8> = DuckString::new(&mut s_raw).as_bytes().to_vec();
                let r = state.execute(&[FunctionValue::Binary(bytes)])
                    .map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::UInt64(v) => v,
                    FunctionValue::UInt32(v) => v as u64,
                    FunctionValue::Int64(v)  => v as u64,
                    FunctionValue::Int32(v)  => v as u64,
                    FunctionValue::Null      => { nulls.push(i); 0 },
                    other => return Err(format!(
                        "BlobToU64Scalar: unexpected variant `{}`",
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
            LogicalTypeHandle::from(LogicalTypeId::UBigint),
        )]
    }
}

// ---- (blob, i64) -> blob ----
fn register_blob_i64_to_blob(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobI64ToBlobScalar>(sql_name, &def)
}
struct BlobI64ToBlobScalar;
impl VScalar for BlobI64ToBlobScalar {
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
        let r1 = unsafe { v1.as_slice_with_len::<i64>(n) };
        let mut out = output.flat_vector();
        let propagates_null = state.propagates_null();
        for i in 0..n {
            if propagates_null
                && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64))
            {
                out.set_null(i); continue;
            }
            let mut s_raw = r0[i];
            let bytes: Vec<u8> = DuckString::new(&mut s_raw).as_bytes().to_vec();
            let r = state.execute(&[
                FunctionValue::Binary(bytes),
                FunctionValue::Int64(r1[i]),
            ]).map_err(|e| format!("{e:?}"))?;
            insert_blob_result::<Self>(&mut out, i, r)?;
        }
        Ok(())
    }
    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![
                LogicalTypeHandle::from(LogicalTypeId::Blob),
                LogicalTypeHandle::from(LogicalTypeId::Bigint),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Blob),
        )]
    }
}

// ---- (blob, i64) -> bool ----
fn register_blob_i64_to_bool(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobI64ToBoolScalar>(sql_name, &def)
}
struct BlobI64ToBoolScalar;
impl VScalar for BlobI64ToBoolScalar {
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
        let r1 = unsafe { v1.as_slice_with_len::<i64>(n) };
        let mut out = output.flat_vector();
        let mut nulls: Vec<usize> = Vec::new();
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<bool>(n) };
            for i in 0..n {
                if propagates_null
                    && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64))
                {
                    nulls.push(i); continue;
                }
                let mut s_raw = r0[i];
                let bytes: Vec<u8> = DuckString::new(&mut s_raw).as_bytes().to_vec();
                let r = state.execute(&[
                    FunctionValue::Binary(bytes),
                    FunctionValue::Int64(r1[i]),
                ]).map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Boolean(b) => b,
                    FunctionValue::Null       => { nulls.push(i); false },
                    other => return Err(format!(
                        "BlobI64ToBoolScalar: unexpected variant `{}`",
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
                LogicalTypeHandle::from(LogicalTypeId::Bigint),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Boolean),
        )]
    }
}

// ---- (blob, i64) -> f64 ----
fn register_blob_i64_to_f64(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobI64ToF64Scalar>(sql_name, &def)
}
struct BlobI64ToF64Scalar;
impl VScalar for BlobI64ToF64Scalar {
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
        let r1 = unsafe { v1.as_slice_with_len::<i64>(n) };
        let mut out = output.flat_vector();
        let mut nulls: Vec<usize> = Vec::new();
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<f64>(n) };
            for i in 0..n {
                if propagates_null
                    && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64))
                {
                    nulls.push(i); continue;
                }
                let mut s_raw = r0[i];
                let bytes: Vec<u8> = DuckString::new(&mut s_raw).as_bytes().to_vec();
                let r = state.execute(&[
                    FunctionValue::Binary(bytes),
                    FunctionValue::Int64(r1[i]),
                ]).map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Float64(v) => v,
                    FunctionValue::Float32(v) => v as f64,
                    FunctionValue::Int64(v)   => v as f64,
                    FunctionValue::Null       => { nulls.push(i); 0.0 },
                    other => return Err(format!(
                        "BlobI64ToF64Scalar: unexpected variant `{}`",
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
                LogicalTypeHandle::from(LogicalTypeId::Bigint),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Double),
        )]
    }
}

// ---- (blob, f64) -> bool ----
fn register_blob_f64_to_bool(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobF64ToBoolScalar>(sql_name, &def)
}
struct BlobF64ToBoolScalar;
impl VScalar for BlobF64ToBoolScalar {
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
        let mut nulls: Vec<usize> = Vec::new();
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<bool>(n) };
            for i in 0..n {
                if propagates_null
                    && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64))
                {
                    nulls.push(i); continue;
                }
                let mut s_raw = r0[i];
                let bytes: Vec<u8> = DuckString::new(&mut s_raw).as_bytes().to_vec();
                let r = state.execute(&[
                    FunctionValue::Binary(bytes),
                    FunctionValue::Float64(r1[i]),
                ]).map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Boolean(b) => b,
                    FunctionValue::Null       => { nulls.push(i); false },
                    other => return Err(format!(
                        "BlobF64ToBoolScalar: unexpected variant `{}`",
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
                LogicalTypeHandle::from(LogicalTypeId::Double),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Boolean),
        )]
    }
}

// ---- (blob, blob) -> i64 ----
fn register_blob_blob_to_i64(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobBlobToI64Scalar>(sql_name, &def)
}
struct BlobBlobToI64Scalar;
impl VScalar for BlobBlobToI64Scalar {
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
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<i64>(n) };
            for i in 0..n {
                if propagates_null
                    && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64))
                {
                    nulls.push(i); continue;
                }
                let mut s0 = r0[i];
                let mut s1 = r1[i];
                let b0: Vec<u8> = DuckString::new(&mut s0).as_bytes().to_vec();
                let b1: Vec<u8> = DuckString::new(&mut s1).as_bytes().to_vec();
                let r = state.execute(&[
                    FunctionValue::Binary(b0),
                    FunctionValue::Binary(b1),
                ]).map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Int64(v)  => v,
                    FunctionValue::Int32(v)  => v as i64,
                    FunctionValue::UInt64(v) => v as i64,
                    FunctionValue::Null      => { nulls.push(i); 0 },
                    other => return Err(format!(
                        "BlobBlobToI64Scalar: unexpected variant `{}`",
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
            LogicalTypeHandle::from(LogicalTypeId::Bigint),
        )]
    }
}

// ---- (blob, i64, i64) -> blob ----
fn register_blob_i64_i64_to_blob(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobI64I64ToBlobScalar>(sql_name, &def)
}
struct BlobI64I64ToBlobScalar;
impl VScalar for BlobI64I64ToBlobScalar {
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
        let r1 = unsafe { v1.as_slice_with_len::<i64>(n) };
        let r2 = unsafe { v2.as_slice_with_len::<i64>(n) };
        let mut out = output.flat_vector();
        let propagates_null = state.propagates_null();
        for i in 0..n {
            if propagates_null
                && (v0.row_is_null(i as u64)
                    || v1.row_is_null(i as u64)
                    || v2.row_is_null(i as u64))
            {
                out.set_null(i); continue;
            }
            let mut s_raw = r0[i];
            let bytes: Vec<u8> = DuckString::new(&mut s_raw).as_bytes().to_vec();
            let r = state.execute(&[
                FunctionValue::Binary(bytes),
                FunctionValue::Int64(r1[i]),
                FunctionValue::Int64(r2[i]),
            ]).map_err(|e| format!("{e:?}"))?;
            insert_blob_result::<Self>(&mut out, i, r)?;
        }
        Ok(())
    }
    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![
                LogicalTypeHandle::from(LogicalTypeId::Blob),
                LogicalTypeHandle::from(LogicalTypeId::Bigint),
                LogicalTypeHandle::from(LogicalTypeId::Bigint),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Blob),
        )]
    }
}

// ---- (f64, f64, f64, f64, i64, i64) -> blob ----
//
// stbox_make-style constructor: 4 spatial bounds + 2 time
// bounds, no leading geometry blob.
fn register_4f64_2i64_to_blob(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<F4I2ToBlobScalar>(sql_name, &def)
}
struct F4I2ToBlobScalar;
impl VScalar for F4I2ToBlobScalar {
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
        let v3 = input.flat_vector(3);
        let v4 = input.flat_vector(4);
        let v5 = input.flat_vector(5);
        let r0 = unsafe { v0.as_slice_with_len::<f64>(n) };
        let r1 = unsafe { v1.as_slice_with_len::<f64>(n) };
        let r2 = unsafe { v2.as_slice_with_len::<f64>(n) };
        let r3 = unsafe { v3.as_slice_with_len::<f64>(n) };
        let r4 = unsafe { v4.as_slice_with_len::<i64>(n) };
        let r5 = unsafe { v5.as_slice_with_len::<i64>(n) };
        let mut out = output.flat_vector();
        let propagates_null = state.propagates_null();
        for i in 0..n {
            if propagates_null
                && (v0.row_is_null(i as u64)
                    || v1.row_is_null(i as u64)
                    || v2.row_is_null(i as u64)
                    || v3.row_is_null(i as u64)
                    || v4.row_is_null(i as u64)
                    || v5.row_is_null(i as u64))
            {
                out.set_null(i); continue;
            }
            let r = state.execute(&[
                FunctionValue::Float64(r0[i]),
                FunctionValue::Float64(r1[i]),
                FunctionValue::Float64(r2[i]),
                FunctionValue::Float64(r3[i]),
                FunctionValue::Int64(r4[i]),
                FunctionValue::Int64(r5[i]),
            ]).map_err(|e| format!("{e:?}"))?;
            insert_blob_result::<Self>(&mut out, i, r)?;
        }
        Ok(())
    }
    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![
                LogicalTypeHandle::from(LogicalTypeId::Double),
                LogicalTypeHandle::from(LogicalTypeId::Double),
                LogicalTypeHandle::from(LogicalTypeId::Double),
                LogicalTypeHandle::from(LogicalTypeId::Double),
                LogicalTypeHandle::from(LogicalTypeId::Bigint),
                LogicalTypeHandle::from(LogicalTypeId::Bigint),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Blob),
        )]
    }
}

// ---- Phase 4f helpers — last long-tail mobilitydb shapes ----

// ---- (blob, f64, f64) -> bool ----
fn register_blob_f64_f64_to_bool(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobF64F64ToBoolScalar>(sql_name, &def)
}
struct BlobF64F64ToBoolScalar;
impl VScalar for BlobF64F64ToBoolScalar {
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
        let r1 = unsafe { v1.as_slice_with_len::<f64>(n) };
        let r2 = unsafe { v2.as_slice_with_len::<f64>(n) };
        let mut out = output.flat_vector();
        let mut nulls: Vec<usize> = Vec::new();
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<bool>(n) };
            for i in 0..n {
                if propagates_null
                    && (v0.row_is_null(i as u64)
                        || v1.row_is_null(i as u64)
                        || v2.row_is_null(i as u64))
                {
                    nulls.push(i); continue;
                }
                let mut s_raw = r0[i];
                let bytes: Vec<u8> = DuckString::new(&mut s_raw).as_bytes().to_vec();
                let r = state.execute(&[
                    FunctionValue::Binary(bytes),
                    FunctionValue::Float64(r1[i]),
                    FunctionValue::Float64(r2[i]),
                ]).map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Boolean(b) => b,
                    FunctionValue::Null       => { nulls.push(i); false },
                    other => return Err(format!(
                        "BlobF64F64ToBoolScalar: unexpected variant `{}`",
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
                LogicalTypeHandle::from(LogicalTypeId::Double),
                LogicalTypeHandle::from(LogicalTypeId::Double),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Boolean),
        )]
    }
}

// ---- (blob, i64, i64) -> i32 ----
fn register_blob_i64_i64_to_i32(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobI64I64ToI32Scalar>(sql_name, &def)
}
struct BlobI64I64ToI32Scalar;
impl VScalar for BlobI64I64ToI32Scalar {
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
        let r1 = unsafe { v1.as_slice_with_len::<i64>(n) };
        let r2 = unsafe { v2.as_slice_with_len::<i64>(n) };
        let mut out = output.flat_vector();
        let mut nulls: Vec<usize> = Vec::new();
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<i32>(n) };
            for i in 0..n {
                if propagates_null
                    && (v0.row_is_null(i as u64)
                        || v1.row_is_null(i as u64)
                        || v2.row_is_null(i as u64))
                {
                    nulls.push(i); continue;
                }
                let mut s_raw = r0[i];
                let bytes: Vec<u8> = DuckString::new(&mut s_raw).as_bytes().to_vec();
                let r = state.execute(&[
                    FunctionValue::Binary(bytes),
                    FunctionValue::Int64(r1[i]),
                    FunctionValue::Int64(r2[i]),
                ]).map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Int32(v) => v,
                    FunctionValue::Int64(v) => v as i32,
                    FunctionValue::UInt32(v) => v as i32,
                    FunctionValue::Null      => { nulls.push(i); 0 },
                    other => return Err(format!(
                        "BlobI64I64ToI32Scalar: unexpected variant `{}`",
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
                LogicalTypeHandle::from(LogicalTypeId::Bigint),
                LogicalTypeHandle::from(LogicalTypeId::Bigint),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Integer),
        )]
    }
}

// ---- (blob, f64, i64) -> i32 ----
fn register_blob_f64_i64_to_i32(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobF64I64ToI32Scalar>(sql_name, &def)
}
struct BlobF64I64ToI32Scalar;
impl VScalar for BlobF64I64ToI32Scalar {
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
        let r1 = unsafe { v1.as_slice_with_len::<f64>(n) };
        let r2 = unsafe { v2.as_slice_with_len::<i64>(n) };
        let mut out = output.flat_vector();
        let mut nulls: Vec<usize> = Vec::new();
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<i32>(n) };
            for i in 0..n {
                if propagates_null
                    && (v0.row_is_null(i as u64)
                        || v1.row_is_null(i as u64)
                        || v2.row_is_null(i as u64))
                {
                    nulls.push(i); continue;
                }
                let mut s_raw = r0[i];
                let bytes: Vec<u8> = DuckString::new(&mut s_raw).as_bytes().to_vec();
                let r = state.execute(&[
                    FunctionValue::Binary(bytes),
                    FunctionValue::Float64(r1[i]),
                    FunctionValue::Int64(r2[i]),
                ]).map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Int32(v) => v,
                    FunctionValue::Int64(v) => v as i32,
                    FunctionValue::Null     => { nulls.push(i); 0 },
                    other => return Err(format!(
                        "BlobF64I64ToI32Scalar: unexpected variant `{}`",
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
                LogicalTypeHandle::from(LogicalTypeId::Double),
                LogicalTypeHandle::from(LogicalTypeId::Bigint),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Integer),
        )]
    }
}

// ---- (i64, i64, bool, bool) -> blob ----
// tstzspan_make-style constructor: two timestamp bounds + two
// inclusivity flags.
fn register_2i64_2bool_to_blob(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<I64I64BoolBoolToBlobScalar>(sql_name, &def)
}
struct I64I64BoolBoolToBlobScalar;
impl VScalar for I64I64BoolBoolToBlobScalar {
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
        let v3 = input.flat_vector(3);
        let r0 = unsafe { v0.as_slice_with_len::<i64>(n) };
        let r1 = unsafe { v1.as_slice_with_len::<i64>(n) };
        let r2 = unsafe { v2.as_slice_with_len::<bool>(n) };
        let r3 = unsafe { v3.as_slice_with_len::<bool>(n) };
        let mut out = output.flat_vector();
        let propagates_null = state.propagates_null();
        for i in 0..n {
            if propagates_null
                && (v0.row_is_null(i as u64)
                    || v1.row_is_null(i as u64)
                    || v2.row_is_null(i as u64)
                    || v3.row_is_null(i as u64))
            {
                out.set_null(i); continue;
            }
            let r = state.execute(&[
                FunctionValue::Int64(r0[i]),
                FunctionValue::Int64(r1[i]),
                FunctionValue::Boolean(r2[i]),
                FunctionValue::Boolean(r3[i]),
            ]).map_err(|e| format!("{e:?}"))?;
            insert_blob_result::<Self>(&mut out, i, r)?;
        }
        Ok(())
    }
    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![
                LogicalTypeHandle::from(LogicalTypeId::Bigint),
                LogicalTypeHandle::from(LogicalTypeId::Bigint),
                LogicalTypeHandle::from(LogicalTypeId::Boolean),
                LogicalTypeHandle::from(LogicalTypeId::Boolean),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Blob),
        )]
    }
}

// ---- (i32, i32, bool, bool) -> blob ----
// datespan_make-style constructor.
fn register_2i32_2bool_to_blob(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<I32I32BoolBoolToBlobScalar>(sql_name, &def)
}
struct I32I32BoolBoolToBlobScalar;
impl VScalar for I32I32BoolBoolToBlobScalar {
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
        let v3 = input.flat_vector(3);
        let r0 = unsafe { v0.as_slice_with_len::<i32>(n) };
        let r1 = unsafe { v1.as_slice_with_len::<i32>(n) };
        let r2 = unsafe { v2.as_slice_with_len::<bool>(n) };
        let r3 = unsafe { v3.as_slice_with_len::<bool>(n) };
        let mut out = output.flat_vector();
        let propagates_null = state.propagates_null();
        for i in 0..n {
            if propagates_null
                && (v0.row_is_null(i as u64)
                    || v1.row_is_null(i as u64)
                    || v2.row_is_null(i as u64)
                    || v3.row_is_null(i as u64))
            {
                out.set_null(i); continue;
            }
            let r = state.execute(&[
                FunctionValue::Int32(r0[i]),
                FunctionValue::Int32(r1[i]),
                FunctionValue::Boolean(r2[i]),
                FunctionValue::Boolean(r3[i]),
            ]).map_err(|e| format!("{e:?}"))?;
            insert_blob_result::<Self>(&mut out, i, r)?;
        }
        Ok(())
    }
    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![
                LogicalTypeHandle::from(LogicalTypeId::Integer),
                LogicalTypeHandle::from(LogicalTypeId::Integer),
                LogicalTypeHandle::from(LogicalTypeId::Boolean),
                LogicalTypeHandle::from(LogicalTypeId::Boolean),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Blob),
        )]
    }
}

// ---- (blob, u32) -> i64 ----
fn register_blob_u32_to_i64(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobU32ToI64Scalar>(sql_name, &def)
}
struct BlobU32ToI64Scalar;
impl VScalar for BlobU32ToI64Scalar {
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
        let mut nulls: Vec<usize> = Vec::new();
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<i64>(n) };
            for i in 0..n {
                if propagates_null
                    && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64))
                {
                    nulls.push(i); continue;
                }
                let mut s_raw = r0[i];
                let bytes: Vec<u8> = DuckString::new(&mut s_raw).as_bytes().to_vec();
                let r = state.execute(&[
                    FunctionValue::Binary(bytes),
                    FunctionValue::UInt32(r1[i]),
                ]).map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Int64(v)  => v,
                    FunctionValue::Int32(v)  => v as i64,
                    FunctionValue::UInt64(v) => v as i64,
                    FunctionValue::Null      => { nulls.push(i); 0 },
                    other => return Err(format!(
                        "BlobU32ToI64Scalar: unexpected variant `{}`",
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
                LogicalTypeHandle::from(LogicalTypeId::UInteger),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Bigint),
        )]
    }
}

// ---- (blob, blob, u32) -> blob ----
fn register_blob_blob_u32_to_blob(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobBlobU32ToBlobScalar>(sql_name, &def)
}
struct BlobBlobU32ToBlobScalar;
impl VScalar for BlobBlobU32ToBlobScalar {
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
        let r2 = unsafe { v2.as_slice_with_len::<u32>(n) };
        let mut out = output.flat_vector();
        let propagates_null = state.propagates_null();
        for i in 0..n {
            if propagates_null
                && (v0.row_is_null(i as u64)
                    || v1.row_is_null(i as u64)
                    || v2.row_is_null(i as u64))
            {
                out.set_null(i); continue;
            }
            let mut s0 = r0[i];
            let mut s1 = r1[i];
            let b0: Vec<u8> = DuckString::new(&mut s0).as_bytes().to_vec();
            let b1: Vec<u8> = DuckString::new(&mut s1).as_bytes().to_vec();
            let r = state.execute(&[
                FunctionValue::Binary(b0),
                FunctionValue::Binary(b1),
                FunctionValue::UInt32(r2[i]),
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
                LogicalTypeHandle::from(LogicalTypeId::UInteger),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Blob),
        )]
    }
}

// ---- Phase 4g helpers — ttext-driven shapes ----

// ---- (blob, text) -> bool ----
fn register_blob_text_to_bool(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobTextToBoolScalar>(sql_name, &def)
}
struct BlobTextToBoolScalar;
impl VScalar for BlobTextToBoolScalar {
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
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<bool>(n) };
            for i in 0..n {
                if propagates_null
                    && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64))
                {
                    nulls.push(i); continue;
                }
                let mut s0 = r0[i];
                let mut s1 = r1[i];
                let bytes: Vec<u8> = DuckString::new(&mut s0).as_bytes().to_vec();
                let s: String = DuckString::new(&mut s1).as_str().to_string();
                let r = state.execute(&[
                    FunctionValue::Binary(bytes),
                    FunctionValue::String(s),
                ]).map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Boolean(b) => b,
                    FunctionValue::Null       => { nulls.push(i); false },
                    other => return Err(format!(
                        "BlobTextToBoolScalar: unexpected variant `{}`",
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
                LogicalTypeHandle::from(LogicalTypeId::Varchar),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Boolean),
        )]
    }
}

// ---- (blob, blob) -> text ----
fn register_blob_blob_to_text(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobBlobToTextScalar>(sql_name, &def)
}
struct BlobBlobToTextScalar;
impl VScalar for BlobBlobToTextScalar {
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
        let propagates_null = state.propagates_null();
        for i in 0..n {
            if propagates_null
                && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64))
            {
                out.set_null(i); continue;
            }
            let mut s0 = r0[i];
            let mut s1 = r1[i];
            let b0: Vec<u8> = DuckString::new(&mut s0).as_bytes().to_vec();
            let b1: Vec<u8> = DuckString::new(&mut s1).as_bytes().to_vec();
            let r = state.execute(&[
                FunctionValue::Binary(b0),
                FunctionValue::Binary(b1),
            ]).map_err(|e| format!("{e:?}"))?;
            match r {
                FunctionValue::String(s) => out.insert(i, s.as_str()),
                FunctionValue::Binary(b) => out.insert(i, b.as_slice()),
                FunctionValue::Null      => out.set_null(i),
                other => return Err(format!(
                    "BlobBlobToTextScalar: unexpected variant `{}`",
                    other.type_name()
                ).into()),
            }
        }
        Ok(())
    }
    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![
                LogicalTypeHandle::from(LogicalTypeId::Blob),
                LogicalTypeHandle::from(LogicalTypeId::Blob),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Varchar),
        )]
    }
}

// ---- (blob, i64) -> text ----
fn register_blob_i64_to_text(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobI64ToTextScalar>(sql_name, &def)
}
struct BlobI64ToTextScalar;
impl VScalar for BlobI64ToTextScalar {
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
        let r1 = unsafe { v1.as_slice_with_len::<i64>(n) };
        let mut out = output.flat_vector();
        let propagates_null = state.propagates_null();
        for i in 0..n {
            if propagates_null
                && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64))
            {
                out.set_null(i); continue;
            }
            let mut s_raw = r0[i];
            let bytes: Vec<u8> = DuckString::new(&mut s_raw).as_bytes().to_vec();
            let r = state.execute(&[
                FunctionValue::Binary(bytes),
                FunctionValue::Int64(r1[i]),
            ]).map_err(|e| format!("{e:?}"))?;
            match r {
                FunctionValue::String(s) => out.insert(i, s.as_str()),
                FunctionValue::Binary(b) => out.insert(i, b.as_slice()),
                FunctionValue::Null      => out.set_null(i),
                other => return Err(format!(
                    "BlobI64ToTextScalar: unexpected variant `{}`",
                    other.type_name()
                ).into()),
            }
        }
        Ok(())
    }
    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![
                LogicalTypeHandle::from(LogicalTypeId::Blob),
                LogicalTypeHandle::from(LogicalTypeId::Bigint),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Varchar),
        )]
    }
}

// ---- (blob, text, text) -> blob ----
fn register_blob_text_text_to_blob(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobTextTextToBlobScalar>(sql_name, &def)
}
struct BlobTextTextToBlobScalar;
impl VScalar for BlobTextTextToBlobScalar {
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
        let r2 = unsafe { v2.as_slice_with_len::<duckdb_string_t>(n) };
        let mut out = output.flat_vector();
        let propagates_null = state.propagates_null();
        for i in 0..n {
            if propagates_null
                && (v0.row_is_null(i as u64)
                    || v1.row_is_null(i as u64)
                    || v2.row_is_null(i as u64))
            {
                out.set_null(i); continue;
            }
            let mut s0 = r0[i];
            let mut s1 = r1[i];
            let mut s2 = r2[i];
            let bytes: Vec<u8> = DuckString::new(&mut s0).as_bytes().to_vec();
            let t1: String = DuckString::new(&mut s1).as_str().to_string();
            let t2: String = DuckString::new(&mut s2).as_str().to_string();
            let r = state.execute(&[
                FunctionValue::Binary(bytes),
                FunctionValue::String(t1),
                FunctionValue::String(t2),
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
                LogicalTypeHandle::from(LogicalTypeId::Varchar),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Blob),
        )]
    }
}

// ---- (text, blob) -> blob ----
// Reversed-arg constructors (e.g. ttext-prepend-str where the
// first arg is the prefix string and the second is the
// sequence).
fn register_text_blob_to_blob(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<TextBlobToBlobScalar>(sql_name, &def)
}
struct TextBlobToBlobScalar;
impl VScalar for TextBlobToBlobScalar {
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
        let propagates_null = state.propagates_null();
        for i in 0..n {
            if propagates_null
                && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64))
            {
                out.set_null(i); continue;
            }
            let mut s0 = r0[i];
            let mut s1 = r1[i];
            let t: String = DuckString::new(&mut s0).as_str().to_string();
            let bytes: Vec<u8> = DuckString::new(&mut s1).as_bytes().to_vec();
            let r = state.execute(&[
                FunctionValue::String(t),
                FunctionValue::Binary(bytes),
            ]).map_err(|e| format!("{e:?}"))?;
            insert_blob_result::<Self>(&mut out, i, r)?;
        }
        Ok(())
    }
    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![
                LogicalTypeHandle::from(LogicalTypeId::Varchar),
                LogicalTypeHandle::from(LogicalTypeId::Blob),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Blob),
        )]
    }
}

// ---- Phase 4h helpers — complete-coverage tail ----


// ---- (binary, binary) -> uint32 ----
fn register_blob_blob_to_u32(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobBlobToU32Scalar>(sql_name, &def)
}
struct BlobBlobToU32Scalar;
impl VScalar for BlobBlobToU32Scalar {
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
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<u32>(n) };
            for i in 0..n {
                if propagates_null && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64)) { nulls.push(i); continue; }
                let mut s_0 = r0[i];
                let bytes_0: Vec<u8> = DuckString::new(&mut s_0).as_bytes().to_vec();
                let mut s_1 = r1[i];
                let bytes_1: Vec<u8> = DuckString::new(&mut s_1).as_bytes().to_vec();
                let r = state.execute(&[
                    FunctionValue::Binary(bytes_0),
                    FunctionValue::Binary(bytes_1),
                ]).map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::UInt32(v) => v,
                    FunctionValue::UInt64(v) => v as u32,
                    FunctionValue::Int32(v) => v as u32,
                    FunctionValue::Int64(v) => v as u32,
                    FunctionValue::Null      => { nulls.push(i); 0 },
                    other => return Err(format!(
                        "BlobBlobToU32Scalar: unexpected variant `{}`",
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
            LogicalTypeHandle::from(LogicalTypeId::UInteger),
        )]
    }
}

// ---- (binary, binary, float64) -> float64 ----
fn register_blob_blob_f64_to_f64(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobBlobF64ToF64Scalar>(sql_name, &def)
}
struct BlobBlobF64ToF64Scalar;
impl VScalar for BlobBlobF64ToF64Scalar {
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
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<f64>(n) };
            for i in 0..n {
                if propagates_null && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64) || v2.row_is_null(i as u64)) { nulls.push(i); continue; }
                let mut s_0 = r0[i];
                let bytes_0: Vec<u8> = DuckString::new(&mut s_0).as_bytes().to_vec();
                let mut s_1 = r1[i];
                let bytes_1: Vec<u8> = DuckString::new(&mut s_1).as_bytes().to_vec();
                let r = state.execute(&[
                    FunctionValue::Binary(bytes_0),
                    FunctionValue::Binary(bytes_1),
                    FunctionValue::Float64(r2[i]),
                ]).map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Float64(v) => v,
                    FunctionValue::Float32(v) => v as f64,
                    FunctionValue::Int64(v) => v as f64,
                    FunctionValue::Int32(v) => v as f64,
                    FunctionValue::Null      => { nulls.push(i); 0.0 },
                    other => return Err(format!(
                        "BlobBlobF64ToF64Scalar: unexpected variant `{}`",
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
            LogicalTypeHandle::from(LogicalTypeId::Double),
        )]
    }
}

// ---- (binary, binary, float64, float64) -> float64 ----
fn register_blob_blob_f64_f64_to_f64(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobBlobF64F64ToF64Scalar>(sql_name, &def)
}
struct BlobBlobF64F64ToF64Scalar;
impl VScalar for BlobBlobF64F64ToF64Scalar {
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
        let v3 = input.flat_vector(3);
        let r0 = unsafe { v0.as_slice_with_len::<duckdb_string_t>(n) };
        let r1 = unsafe { v1.as_slice_with_len::<duckdb_string_t>(n) };
        let r2 = unsafe { v2.as_slice_with_len::<f64>(n) };
        let r3 = unsafe { v3.as_slice_with_len::<f64>(n) };
        let mut out = output.flat_vector();
        let mut nulls: Vec<usize> = Vec::new();
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<f64>(n) };
            for i in 0..n {
                if propagates_null && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64) || v2.row_is_null(i as u64) || v3.row_is_null(i as u64)) { nulls.push(i); continue; }
                let mut s_0 = r0[i];
                let bytes_0: Vec<u8> = DuckString::new(&mut s_0).as_bytes().to_vec();
                let mut s_1 = r1[i];
                let bytes_1: Vec<u8> = DuckString::new(&mut s_1).as_bytes().to_vec();
                let r = state.execute(&[
                    FunctionValue::Binary(bytes_0),
                    FunctionValue::Binary(bytes_1),
                    FunctionValue::Float64(r2[i]),
                    FunctionValue::Float64(r3[i]),
                ]).map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Float64(v) => v,
                    FunctionValue::Float32(v) => v as f64,
                    FunctionValue::Int64(v) => v as f64,
                    FunctionValue::Int32(v) => v as f64,
                    FunctionValue::Null      => { nulls.push(i); 0.0 },
                    other => return Err(format!(
                        "BlobBlobF64F64ToF64Scalar: unexpected variant `{}`",
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
                LogicalTypeHandle::from(LogicalTypeId::Double),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Double),
        )]
    }
}

// ---- (binary, binary, float64, uint32) -> float64 ----
fn register_blob_blob_f64_u32_to_f64(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobBlobF64U32ToF64Scalar>(sql_name, &def)
}
struct BlobBlobF64U32ToF64Scalar;
impl VScalar for BlobBlobF64U32ToF64Scalar {
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
        let v3 = input.flat_vector(3);
        let r0 = unsafe { v0.as_slice_with_len::<duckdb_string_t>(n) };
        let r1 = unsafe { v1.as_slice_with_len::<duckdb_string_t>(n) };
        let r2 = unsafe { v2.as_slice_with_len::<f64>(n) };
        let r3 = unsafe { v3.as_slice_with_len::<u32>(n) };
        let mut out = output.flat_vector();
        let mut nulls: Vec<usize> = Vec::new();
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<f64>(n) };
            for i in 0..n {
                if propagates_null && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64) || v2.row_is_null(i as u64) || v3.row_is_null(i as u64)) { nulls.push(i); continue; }
                let mut s_0 = r0[i];
                let bytes_0: Vec<u8> = DuckString::new(&mut s_0).as_bytes().to_vec();
                let mut s_1 = r1[i];
                let bytes_1: Vec<u8> = DuckString::new(&mut s_1).as_bytes().to_vec();
                let r = state.execute(&[
                    FunctionValue::Binary(bytes_0),
                    FunctionValue::Binary(bytes_1),
                    FunctionValue::Float64(r2[i]),
                    FunctionValue::UInt32(r3[i]),
                ]).map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Float64(v) => v,
                    FunctionValue::Float32(v) => v as f64,
                    FunctionValue::Int64(v) => v as f64,
                    FunctionValue::Int32(v) => v as f64,
                    FunctionValue::Null      => { nulls.push(i); 0.0 },
                    other => return Err(format!(
                        "BlobBlobF64U32ToF64Scalar: unexpected variant `{}`",
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
                LogicalTypeHandle::from(LogicalTypeId::UInteger),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Double),
        )]
    }
}

// ---- (binary, binary, int64) -> float64 ----
fn register_blob_blob_i64_to_f64(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobBlobI64ToF64Scalar>(sql_name, &def)
}
struct BlobBlobI64ToF64Scalar;
impl VScalar for BlobBlobI64ToF64Scalar {
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
        let r2 = unsafe { v2.as_slice_with_len::<i64>(n) };
        let mut out = output.flat_vector();
        let mut nulls: Vec<usize> = Vec::new();
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<f64>(n) };
            for i in 0..n {
                if propagates_null && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64) || v2.row_is_null(i as u64)) { nulls.push(i); continue; }
                let mut s_0 = r0[i];
                let bytes_0: Vec<u8> = DuckString::new(&mut s_0).as_bytes().to_vec();
                let mut s_1 = r1[i];
                let bytes_1: Vec<u8> = DuckString::new(&mut s_1).as_bytes().to_vec();
                let r = state.execute(&[
                    FunctionValue::Binary(bytes_0),
                    FunctionValue::Binary(bytes_1),
                    FunctionValue::Int64(r2[i]),
                ]).map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Float64(v) => v,
                    FunctionValue::Float32(v) => v as f64,
                    FunctionValue::Int64(v) => v as f64,
                    FunctionValue::Int32(v) => v as f64,
                    FunctionValue::Null      => { nulls.push(i); 0.0 },
                    other => return Err(format!(
                        "BlobBlobI64ToF64Scalar: unexpected variant `{}`",
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
                LogicalTypeHandle::from(LogicalTypeId::Bigint),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Double),
        )]
    }
}

// ---- (binary, boolean) -> binary ----
fn register_blob_bool_to_blob(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobBoolToBlobScalar>(sql_name, &def)
}
struct BlobBoolToBlobScalar;
impl VScalar for BlobBoolToBlobScalar {
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
        let r1 = unsafe { v1.as_slice_with_len::<bool>(n) };
        let mut out = output.flat_vector();
        let propagates_null = state.propagates_null();
        for i in 0..n {
            if propagates_null && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64)) { out.set_null(i); continue; }
            let mut s_0 = r0[i];
            let bytes_0: Vec<u8> = DuckString::new(&mut s_0).as_bytes().to_vec();
            let r = state.execute(&[
                FunctionValue::Binary(bytes_0),
                FunctionValue::Boolean(r1[i]),
            ]).map_err(|e| format!("{e:?}"))?;
            insert_blob_result::<Self>(&mut out, i, r)?;
        }
        Ok(())
    }
    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![
                LogicalTypeHandle::from(LogicalTypeId::Blob),
                LogicalTypeHandle::from(LogicalTypeId::Boolean),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Blob),
        )]
    }
}

// ---- (binary, float64) -> int32 ----
fn register_blob_f64_to_i32(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobF64ToI32Scalar>(sql_name, &def)
}
struct BlobF64ToI32Scalar;
impl VScalar for BlobF64ToI32Scalar {
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
        let mut nulls: Vec<usize> = Vec::new();
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<i32>(n) };
            for i in 0..n {
                if propagates_null && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64)) { nulls.push(i); continue; }
                let mut s_0 = r0[i];
                let bytes_0: Vec<u8> = DuckString::new(&mut s_0).as_bytes().to_vec();
                let r = state.execute(&[
                    FunctionValue::Binary(bytes_0),
                    FunctionValue::Float64(r1[i]),
                ]).map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Int32(v) => v,
                    FunctionValue::Int64(v) => v as i32,
                    FunctionValue::UInt32(v) => v as i32,
                    FunctionValue::Null      => { nulls.push(i); 0 },
                    other => return Err(format!(
                        "BlobF64ToI32Scalar: unexpected variant `{}`",
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
                LogicalTypeHandle::from(LogicalTypeId::Double),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Integer),
        )]
    }
}

// ---- (binary, float64, float64) -> float64 ----
fn register_blob_f64_f64_to_f64(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobF64F64ToF64Scalar>(sql_name, &def)
}
struct BlobF64F64ToF64Scalar;
impl VScalar for BlobF64F64ToF64Scalar {
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
        let r1 = unsafe { v1.as_slice_with_len::<f64>(n) };
        let r2 = unsafe { v2.as_slice_with_len::<f64>(n) };
        let mut out = output.flat_vector();
        let mut nulls: Vec<usize> = Vec::new();
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<f64>(n) };
            for i in 0..n {
                if propagates_null && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64) || v2.row_is_null(i as u64)) { nulls.push(i); continue; }
                let mut s_0 = r0[i];
                let bytes_0: Vec<u8> = DuckString::new(&mut s_0).as_bytes().to_vec();
                let r = state.execute(&[
                    FunctionValue::Binary(bytes_0),
                    FunctionValue::Float64(r1[i]),
                    FunctionValue::Float64(r2[i]),
                ]).map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Float64(v) => v,
                    FunctionValue::Float32(v) => v as f64,
                    FunctionValue::Int64(v) => v as f64,
                    FunctionValue::Int32(v) => v as f64,
                    FunctionValue::Null      => { nulls.push(i); 0.0 },
                    other => return Err(format!(
                        "BlobF64F64ToF64Scalar: unexpected variant `{}`",
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
                LogicalTypeHandle::from(LogicalTypeId::Double),
                LogicalTypeHandle::from(LogicalTypeId::Double),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Double),
        )]
    }
}

// ---- (binary, float64, float64, float64) -> binary ----
fn register_blob_f64_f64_f64_to_blob(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobF64F64F64ToBlobScalar>(sql_name, &def)
}
struct BlobF64F64F64ToBlobScalar;
impl VScalar for BlobF64F64F64ToBlobScalar {
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
        let v3 = input.flat_vector(3);
        let r0 = unsafe { v0.as_slice_with_len::<duckdb_string_t>(n) };
        let r1 = unsafe { v1.as_slice_with_len::<f64>(n) };
        let r2 = unsafe { v2.as_slice_with_len::<f64>(n) };
        let r3 = unsafe { v3.as_slice_with_len::<f64>(n) };
        let mut out = output.flat_vector();
        let propagates_null = state.propagates_null();
        for i in 0..n {
            if propagates_null && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64) || v2.row_is_null(i as u64) || v3.row_is_null(i as u64)) { out.set_null(i); continue; }
            let mut s_0 = r0[i];
            let bytes_0: Vec<u8> = DuckString::new(&mut s_0).as_bytes().to_vec();
            let r = state.execute(&[
                FunctionValue::Binary(bytes_0),
                FunctionValue::Float64(r1[i]),
                FunctionValue::Float64(r2[i]),
                FunctionValue::Float64(r3[i]),
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
                LogicalTypeHandle::from(LogicalTypeId::Double),
                LogicalTypeHandle::from(LogicalTypeId::Double),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Blob),
        )]
    }
}

// ---- (binary, float64, float64, float64) -> boolean ----
fn register_blob_f64_f64_f64_to_bool(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobF64F64F64ToBoolScalar>(sql_name, &def)
}
struct BlobF64F64F64ToBoolScalar;
impl VScalar for BlobF64F64F64ToBoolScalar {
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
        let v3 = input.flat_vector(3);
        let r0 = unsafe { v0.as_slice_with_len::<duckdb_string_t>(n) };
        let r1 = unsafe { v1.as_slice_with_len::<f64>(n) };
        let r2 = unsafe { v2.as_slice_with_len::<f64>(n) };
        let r3 = unsafe { v3.as_slice_with_len::<f64>(n) };
        let mut out = output.flat_vector();
        let mut nulls: Vec<usize> = Vec::new();
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<bool>(n) };
            for i in 0..n {
                if propagates_null && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64) || v2.row_is_null(i as u64) || v3.row_is_null(i as u64)) { nulls.push(i); continue; }
                let mut s_0 = r0[i];
                let bytes_0: Vec<u8> = DuckString::new(&mut s_0).as_bytes().to_vec();
                let r = state.execute(&[
                    FunctionValue::Binary(bytes_0),
                    FunctionValue::Float64(r1[i]),
                    FunctionValue::Float64(r2[i]),
                    FunctionValue::Float64(r3[i]),
                ]).map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Boolean(v) => v,
                    FunctionValue::Null      => { nulls.push(i); false },
                    other => return Err(format!(
                        "BlobF64F64F64ToBoolScalar: unexpected variant `{}`",
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
                LogicalTypeHandle::from(LogicalTypeId::Double),
                LogicalTypeHandle::from(LogicalTypeId::Double),
                LogicalTypeHandle::from(LogicalTypeId::Double),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Boolean),
        )]
    }
}

// ---- (binary, float64, float64, float64, int64) -> float64 ----
fn register_blob_f64_f64_f64_i64_to_f64(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobF64F64F64I64ToF64Scalar>(sql_name, &def)
}
struct BlobF64F64F64I64ToF64Scalar;
impl VScalar for BlobF64F64F64I64ToF64Scalar {
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
        let v3 = input.flat_vector(3);
        let v4 = input.flat_vector(4);
        let r0 = unsafe { v0.as_slice_with_len::<duckdb_string_t>(n) };
        let r1 = unsafe { v1.as_slice_with_len::<f64>(n) };
        let r2 = unsafe { v2.as_slice_with_len::<f64>(n) };
        let r3 = unsafe { v3.as_slice_with_len::<f64>(n) };
        let r4 = unsafe { v4.as_slice_with_len::<i64>(n) };
        let mut out = output.flat_vector();
        let mut nulls: Vec<usize> = Vec::new();
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<f64>(n) };
            for i in 0..n {
                if propagates_null && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64) || v2.row_is_null(i as u64) || v3.row_is_null(i as u64) || v4.row_is_null(i as u64)) { nulls.push(i); continue; }
                let mut s_0 = r0[i];
                let bytes_0: Vec<u8> = DuckString::new(&mut s_0).as_bytes().to_vec();
                let r = state.execute(&[
                    FunctionValue::Binary(bytes_0),
                    FunctionValue::Float64(r1[i]),
                    FunctionValue::Float64(r2[i]),
                    FunctionValue::Float64(r3[i]),
                    FunctionValue::Int64(r4[i]),
                ]).map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Float64(v) => v,
                    FunctionValue::Float32(v) => v as f64,
                    FunctionValue::Int64(v) => v as f64,
                    FunctionValue::Int32(v) => v as f64,
                    FunctionValue::Null      => { nulls.push(i); 0.0 },
                    other => return Err(format!(
                        "BlobF64F64F64I64ToF64Scalar: unexpected variant `{}`",
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
                LogicalTypeHandle::from(LogicalTypeId::Double),
                LogicalTypeHandle::from(LogicalTypeId::Double),
                LogicalTypeHandle::from(LogicalTypeId::Double),
                LogicalTypeHandle::from(LogicalTypeId::Bigint),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Double),
        )]
    }
}

// ---- (binary, float64, float64, int64) -> binary ----
fn register_blob_f64_f64_i64_to_blob(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobF64F64I64ToBlobScalar>(sql_name, &def)
}
struct BlobF64F64I64ToBlobScalar;
impl VScalar for BlobF64F64I64ToBlobScalar {
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
        let v3 = input.flat_vector(3);
        let r0 = unsafe { v0.as_slice_with_len::<duckdb_string_t>(n) };
        let r1 = unsafe { v1.as_slice_with_len::<f64>(n) };
        let r2 = unsafe { v2.as_slice_with_len::<f64>(n) };
        let r3 = unsafe { v3.as_slice_with_len::<i64>(n) };
        let mut out = output.flat_vector();
        let propagates_null = state.propagates_null();
        for i in 0..n {
            if propagates_null && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64) || v2.row_is_null(i as u64) || v3.row_is_null(i as u64)) { out.set_null(i); continue; }
            let mut s_0 = r0[i];
            let bytes_0: Vec<u8> = DuckString::new(&mut s_0).as_bytes().to_vec();
            let r = state.execute(&[
                FunctionValue::Binary(bytes_0),
                FunctionValue::Float64(r1[i]),
                FunctionValue::Float64(r2[i]),
                FunctionValue::Int64(r3[i]),
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
                LogicalTypeHandle::from(LogicalTypeId::Double),
                LogicalTypeHandle::from(LogicalTypeId::Bigint),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Blob),
        )]
    }
}

// ---- (binary, float64, float64, int64) -> boolean ----
fn register_blob_f64_f64_i64_to_bool(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobF64F64I64ToBoolScalar>(sql_name, &def)
}
struct BlobF64F64I64ToBoolScalar;
impl VScalar for BlobF64F64I64ToBoolScalar {
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
        let v3 = input.flat_vector(3);
        let r0 = unsafe { v0.as_slice_with_len::<duckdb_string_t>(n) };
        let r1 = unsafe { v1.as_slice_with_len::<f64>(n) };
        let r2 = unsafe { v2.as_slice_with_len::<f64>(n) };
        let r3 = unsafe { v3.as_slice_with_len::<i64>(n) };
        let mut out = output.flat_vector();
        let mut nulls: Vec<usize> = Vec::new();
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<bool>(n) };
            for i in 0..n {
                if propagates_null && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64) || v2.row_is_null(i as u64) || v3.row_is_null(i as u64)) { nulls.push(i); continue; }
                let mut s_0 = r0[i];
                let bytes_0: Vec<u8> = DuckString::new(&mut s_0).as_bytes().to_vec();
                let r = state.execute(&[
                    FunctionValue::Binary(bytes_0),
                    FunctionValue::Float64(r1[i]),
                    FunctionValue::Float64(r2[i]),
                    FunctionValue::Int64(r3[i]),
                ]).map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Boolean(v) => v,
                    FunctionValue::Null      => { nulls.push(i); false },
                    other => return Err(format!(
                        "BlobF64F64I64ToBoolScalar: unexpected variant `{}`",
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
                LogicalTypeHandle::from(LogicalTypeId::Double),
                LogicalTypeHandle::from(LogicalTypeId::Double),
                LogicalTypeHandle::from(LogicalTypeId::Bigint),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Boolean),
        )]
    }
}

// ---- (binary, float64, int64) -> binary ----
fn register_blob_f64_i64_to_blob(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobF64I64ToBlobScalar>(sql_name, &def)
}
struct BlobF64I64ToBlobScalar;
impl VScalar for BlobF64I64ToBlobScalar {
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
        let r1 = unsafe { v1.as_slice_with_len::<f64>(n) };
        let r2 = unsafe { v2.as_slice_with_len::<i64>(n) };
        let mut out = output.flat_vector();
        let propagates_null = state.propagates_null();
        for i in 0..n {
            if propagates_null && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64) || v2.row_is_null(i as u64)) { out.set_null(i); continue; }
            let mut s_0 = r0[i];
            let bytes_0: Vec<u8> = DuckString::new(&mut s_0).as_bytes().to_vec();
            let r = state.execute(&[
                FunctionValue::Binary(bytes_0),
                FunctionValue::Float64(r1[i]),
                FunctionValue::Int64(r2[i]),
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
                LogicalTypeHandle::from(LogicalTypeId::Bigint),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Blob),
        )]
    }
}

// ---- (binary, float64, int64) -> int64 ----
fn register_blob_f64_i64_to_i64(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobF64I64ToI64Scalar>(sql_name, &def)
}
struct BlobF64I64ToI64Scalar;
impl VScalar for BlobF64I64ToI64Scalar {
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
        let r1 = unsafe { v1.as_slice_with_len::<f64>(n) };
        let r2 = unsafe { v2.as_slice_with_len::<i64>(n) };
        let mut out = output.flat_vector();
        let mut nulls: Vec<usize> = Vec::new();
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<i64>(n) };
            for i in 0..n {
                if propagates_null && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64) || v2.row_is_null(i as u64)) { nulls.push(i); continue; }
                let mut s_0 = r0[i];
                let bytes_0: Vec<u8> = DuckString::new(&mut s_0).as_bytes().to_vec();
                let r = state.execute(&[
                    FunctionValue::Binary(bytes_0),
                    FunctionValue::Float64(r1[i]),
                    FunctionValue::Int64(r2[i]),
                ]).map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Int64(v) => v,
                    FunctionValue::Int32(v) => v as i64,
                    FunctionValue::UInt32(v) => v as i64,
                    FunctionValue::UInt64(v) => v as i64,
                    FunctionValue::Null      => { nulls.push(i); 0 },
                    other => return Err(format!(
                        "BlobF64I64ToI64Scalar: unexpected variant `{}`",
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
                LogicalTypeHandle::from(LogicalTypeId::Double),
                LogicalTypeHandle::from(LogicalTypeId::Bigint),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Bigint),
        )]
    }
}

// ---- (binary, float64, uint32) -> int32 ----
fn register_blob_f64_u32_to_i32(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobF64U32ToI32Scalar>(sql_name, &def)
}
struct BlobF64U32ToI32Scalar;
impl VScalar for BlobF64U32ToI32Scalar {
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
        let r1 = unsafe { v1.as_slice_with_len::<f64>(n) };
        let r2 = unsafe { v2.as_slice_with_len::<u32>(n) };
        let mut out = output.flat_vector();
        let mut nulls: Vec<usize> = Vec::new();
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<i32>(n) };
            for i in 0..n {
                if propagates_null && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64) || v2.row_is_null(i as u64)) { nulls.push(i); continue; }
                let mut s_0 = r0[i];
                let bytes_0: Vec<u8> = DuckString::new(&mut s_0).as_bytes().to_vec();
                let r = state.execute(&[
                    FunctionValue::Binary(bytes_0),
                    FunctionValue::Float64(r1[i]),
                    FunctionValue::UInt32(r2[i]),
                ]).map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Int32(v) => v,
                    FunctionValue::Int64(v) => v as i32,
                    FunctionValue::UInt32(v) => v as i32,
                    FunctionValue::Null      => { nulls.push(i); 0 },
                    other => return Err(format!(
                        "BlobF64U32ToI32Scalar: unexpected variant `{}`",
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
                LogicalTypeHandle::from(LogicalTypeId::Double),
                LogicalTypeHandle::from(LogicalTypeId::UInteger),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Integer),
        )]
    }
}

// ---- (binary, int32) -> boolean ----
fn register_blob_i32_to_bool(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobI32ToBoolScalar>(sql_name, &def)
}
struct BlobI32ToBoolScalar;
impl VScalar for BlobI32ToBoolScalar {
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
        let mut nulls: Vec<usize> = Vec::new();
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<bool>(n) };
            for i in 0..n {
                if propagates_null && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64)) { nulls.push(i); continue; }
                let mut s_0 = r0[i];
                let bytes_0: Vec<u8> = DuckString::new(&mut s_0).as_bytes().to_vec();
                let r = state.execute(&[
                    FunctionValue::Binary(bytes_0),
                    FunctionValue::Int32(r1[i]),
                ]).map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Boolean(v) => v,
                    FunctionValue::Null      => { nulls.push(i); false },
                    other => return Err(format!(
                        "BlobI32ToBoolScalar: unexpected variant `{}`",
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
                LogicalTypeHandle::from(LogicalTypeId::Integer),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Boolean),
        )]
    }
}

// ---- (binary, int64) -> int32 ----
fn register_blob_i64_to_i32(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobI64ToI32Scalar>(sql_name, &def)
}
struct BlobI64ToI32Scalar;
impl VScalar for BlobI64ToI32Scalar {
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
        let r1 = unsafe { v1.as_slice_with_len::<i64>(n) };
        let mut out = output.flat_vector();
        let mut nulls: Vec<usize> = Vec::new();
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<i32>(n) };
            for i in 0..n {
                if propagates_null && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64)) { nulls.push(i); continue; }
                let mut s_0 = r0[i];
                let bytes_0: Vec<u8> = DuckString::new(&mut s_0).as_bytes().to_vec();
                let r = state.execute(&[
                    FunctionValue::Binary(bytes_0),
                    FunctionValue::Int64(r1[i]),
                ]).map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Int32(v) => v,
                    FunctionValue::Int64(v) => v as i32,
                    FunctionValue::UInt32(v) => v as i32,
                    FunctionValue::Null      => { nulls.push(i); 0 },
                    other => return Err(format!(
                        "BlobI64ToI32Scalar: unexpected variant `{}`",
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
                LogicalTypeHandle::from(LogicalTypeId::Bigint),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Integer),
        )]
    }
}

// ---- (binary, int64) -> int64 ----
fn register_blob_i64_to_i64(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobI64ToI64Scalar>(sql_name, &def)
}
struct BlobI64ToI64Scalar;
impl VScalar for BlobI64ToI64Scalar {
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
        let r1 = unsafe { v1.as_slice_with_len::<i64>(n) };
        let mut out = output.flat_vector();
        let mut nulls: Vec<usize> = Vec::new();
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<i64>(n) };
            for i in 0..n {
                if propagates_null && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64)) { nulls.push(i); continue; }
                let mut s_0 = r0[i];
                let bytes_0: Vec<u8> = DuckString::new(&mut s_0).as_bytes().to_vec();
                let r = state.execute(&[
                    FunctionValue::Binary(bytes_0),
                    FunctionValue::Int64(r1[i]),
                ]).map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Int64(v) => v,
                    FunctionValue::Int32(v) => v as i64,
                    FunctionValue::UInt32(v) => v as i64,
                    FunctionValue::UInt64(v) => v as i64,
                    FunctionValue::Null      => { nulls.push(i); 0 },
                    other => return Err(format!(
                        "BlobI64ToI64Scalar: unexpected variant `{}`",
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
                LogicalTypeHandle::from(LogicalTypeId::Bigint),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Bigint),
        )]
    }
}

// ---- (binary, int64, int64) -> boolean ----
fn register_blob_i64_i64_to_bool(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobI64I64ToBoolScalar>(sql_name, &def)
}
struct BlobI64I64ToBoolScalar;
impl VScalar for BlobI64I64ToBoolScalar {
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
        let r1 = unsafe { v1.as_slice_with_len::<i64>(n) };
        let r2 = unsafe { v2.as_slice_with_len::<i64>(n) };
        let mut out = output.flat_vector();
        let mut nulls: Vec<usize> = Vec::new();
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<bool>(n) };
            for i in 0..n {
                if propagates_null && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64) || v2.row_is_null(i as u64)) { nulls.push(i); continue; }
                let mut s_0 = r0[i];
                let bytes_0: Vec<u8> = DuckString::new(&mut s_0).as_bytes().to_vec();
                let r = state.execute(&[
                    FunctionValue::Binary(bytes_0),
                    FunctionValue::Int64(r1[i]),
                    FunctionValue::Int64(r2[i]),
                ]).map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Boolean(v) => v,
                    FunctionValue::Null      => { nulls.push(i); false },
                    other => return Err(format!(
                        "BlobI64I64ToBoolScalar: unexpected variant `{}`",
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
                LogicalTypeHandle::from(LogicalTypeId::Bigint),
                LogicalTypeHandle::from(LogicalTypeId::Bigint),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Boolean),
        )]
    }
}

// ---- (binary, int64, int64) -> float64 ----
fn register_blob_i64_i64_to_f64(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobI64I64ToF64Scalar>(sql_name, &def)
}
struct BlobI64I64ToF64Scalar;
impl VScalar for BlobI64I64ToF64Scalar {
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
        let r1 = unsafe { v1.as_slice_with_len::<i64>(n) };
        let r2 = unsafe { v2.as_slice_with_len::<i64>(n) };
        let mut out = output.flat_vector();
        let mut nulls: Vec<usize> = Vec::new();
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<f64>(n) };
            for i in 0..n {
                if propagates_null && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64) || v2.row_is_null(i as u64)) { nulls.push(i); continue; }
                let mut s_0 = r0[i];
                let bytes_0: Vec<u8> = DuckString::new(&mut s_0).as_bytes().to_vec();
                let r = state.execute(&[
                    FunctionValue::Binary(bytes_0),
                    FunctionValue::Int64(r1[i]),
                    FunctionValue::Int64(r2[i]),
                ]).map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Float64(v) => v,
                    FunctionValue::Float32(v) => v as f64,
                    FunctionValue::Int64(v) => v as f64,
                    FunctionValue::Int32(v) => v as f64,
                    FunctionValue::Null      => { nulls.push(i); 0.0 },
                    other => return Err(format!(
                        "BlobI64I64ToF64Scalar: unexpected variant `{}`",
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
                LogicalTypeHandle::from(LogicalTypeId::Bigint),
                LogicalTypeHandle::from(LogicalTypeId::Bigint),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Double),
        )]
    }
}

// ---- (binary, int64, int64) -> int64 ----
fn register_blob_i64_i64_to_i64(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobI64I64ToI64Scalar>(sql_name, &def)
}
struct BlobI64I64ToI64Scalar;
impl VScalar for BlobI64I64ToI64Scalar {
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
        let r1 = unsafe { v1.as_slice_with_len::<i64>(n) };
        let r2 = unsafe { v2.as_slice_with_len::<i64>(n) };
        let mut out = output.flat_vector();
        let mut nulls: Vec<usize> = Vec::new();
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<i64>(n) };
            for i in 0..n {
                if propagates_null && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64) || v2.row_is_null(i as u64)) { nulls.push(i); continue; }
                let mut s_0 = r0[i];
                let bytes_0: Vec<u8> = DuckString::new(&mut s_0).as_bytes().to_vec();
                let r = state.execute(&[
                    FunctionValue::Binary(bytes_0),
                    FunctionValue::Int64(r1[i]),
                    FunctionValue::Int64(r2[i]),
                ]).map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Int64(v) => v,
                    FunctionValue::Int32(v) => v as i64,
                    FunctionValue::UInt32(v) => v as i64,
                    FunctionValue::UInt64(v) => v as i64,
                    FunctionValue::Null      => { nulls.push(i); 0 },
                    other => return Err(format!(
                        "BlobI64I64ToI64Scalar: unexpected variant `{}`",
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
                LogicalTypeHandle::from(LogicalTypeId::Bigint),
                LogicalTypeHandle::from(LogicalTypeId::Bigint),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Bigint),
        )]
    }
}

// ---- (binary, int64, int64) -> text ----
fn register_blob_i64_i64_to_text(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobI64I64ToTextScalar>(sql_name, &def)
}
struct BlobI64I64ToTextScalar;
impl VScalar for BlobI64I64ToTextScalar {
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
        let r1 = unsafe { v1.as_slice_with_len::<i64>(n) };
        let r2 = unsafe { v2.as_slice_with_len::<i64>(n) };
        let mut out = output.flat_vector();
        let propagates_null = state.propagates_null();
        for i in 0..n {
            if propagates_null && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64) || v2.row_is_null(i as u64)) { out.set_null(i); continue; }
            let mut s_0 = r0[i];
            let bytes_0: Vec<u8> = DuckString::new(&mut s_0).as_bytes().to_vec();
            let r = state.execute(&[
                FunctionValue::Binary(bytes_0),
                FunctionValue::Int64(r1[i]),
                FunctionValue::Int64(r2[i]),
            ]).map_err(|e| format!("{e:?}"))?;
            match r {
                FunctionValue::String(s) => out.insert(i, s.as_str()),
                FunctionValue::Binary(b) => out.insert(i, b.as_slice()),
                FunctionValue::Null      => out.set_null(i),
                other => return Err(format!(
                    "BlobI64I64ToTextScalar: unexpected variant `{}`",
                    other.type_name()
                ).into()),
            }
        }
        Ok(())
    }
    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![
                LogicalTypeHandle::from(LogicalTypeId::Blob),
                LogicalTypeHandle::from(LogicalTypeId::Bigint),
                LogicalTypeHandle::from(LogicalTypeId::Bigint),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Varchar),
        )]
    }
}

// ---- (binary, uint32) -> float64 ----
fn register_blob_u32_to_f64(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobU32ToF64Scalar>(sql_name, &def)
}
struct BlobU32ToF64Scalar;
impl VScalar for BlobU32ToF64Scalar {
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
        let mut nulls: Vec<usize> = Vec::new();
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<f64>(n) };
            for i in 0..n {
                if propagates_null && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64)) { nulls.push(i); continue; }
                let mut s_0 = r0[i];
                let bytes_0: Vec<u8> = DuckString::new(&mut s_0).as_bytes().to_vec();
                let r = state.execute(&[
                    FunctionValue::Binary(bytes_0),
                    FunctionValue::UInt32(r1[i]),
                ]).map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Float64(v) => v,
                    FunctionValue::Float32(v) => v as f64,
                    FunctionValue::Int64(v) => v as f64,
                    FunctionValue::Int32(v) => v as f64,
                    FunctionValue::Null      => { nulls.push(i); 0.0 },
                    other => return Err(format!(
                        "BlobU32ToF64Scalar: unexpected variant `{}`",
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
                LogicalTypeHandle::from(LogicalTypeId::UInteger),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Double),
        )]
    }
}

// ---- (binary, uint32) -> int32 ----
fn register_blob_u32_to_i32(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobU32ToI32Scalar>(sql_name, &def)
}
struct BlobU32ToI32Scalar;
impl VScalar for BlobU32ToI32Scalar {
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
        let mut nulls: Vec<usize> = Vec::new();
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<i32>(n) };
            for i in 0..n {
                if propagates_null && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64)) { nulls.push(i); continue; }
                let mut s_0 = r0[i];
                let bytes_0: Vec<u8> = DuckString::new(&mut s_0).as_bytes().to_vec();
                let r = state.execute(&[
                    FunctionValue::Binary(bytes_0),
                    FunctionValue::UInt32(r1[i]),
                ]).map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Int32(v) => v,
                    FunctionValue::Int64(v) => v as i32,
                    FunctionValue::UInt32(v) => v as i32,
                    FunctionValue::Null      => { nulls.push(i); 0 },
                    other => return Err(format!(
                        "BlobU32ToI32Scalar: unexpected variant `{}`",
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
                LogicalTypeHandle::from(LogicalTypeId::UInteger),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Integer),
        )]
    }
}

// ---- (binary, uint32, float64) -> float64 ----
fn register_blob_u32_f64_to_f64(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<BlobU32F64ToF64Scalar>(sql_name, &def)
}
struct BlobU32F64ToF64Scalar;
impl VScalar for BlobU32F64ToF64Scalar {
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
        let r1 = unsafe { v1.as_slice_with_len::<u32>(n) };
        let r2 = unsafe { v2.as_slice_with_len::<f64>(n) };
        let mut out = output.flat_vector();
        let mut nulls: Vec<usize> = Vec::new();
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<f64>(n) };
            for i in 0..n {
                if propagates_null && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64) || v2.row_is_null(i as u64)) { nulls.push(i); continue; }
                let mut s_0 = r0[i];
                let bytes_0: Vec<u8> = DuckString::new(&mut s_0).as_bytes().to_vec();
                let r = state.execute(&[
                    FunctionValue::Binary(bytes_0),
                    FunctionValue::UInt32(r1[i]),
                    FunctionValue::Float64(r2[i]),
                ]).map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Float64(v) => v,
                    FunctionValue::Float32(v) => v as f64,
                    FunctionValue::Int64(v) => v as f64,
                    FunctionValue::Int32(v) => v as f64,
                    FunctionValue::Null      => { nulls.push(i); 0.0 },
                    other => return Err(format!(
                        "BlobU32F64ToF64Scalar: unexpected variant `{}`",
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
                LogicalTypeHandle::from(LogicalTypeId::UInteger),
                LogicalTypeHandle::from(LogicalTypeId::Double),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Double),
        )]
    }
}

// ---- (float64, float64) -> float64 ----
fn register_f64_f64_to_f64(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<F64F64ToF64Scalar>(sql_name, &def)
}
struct F64F64ToF64Scalar;
impl VScalar for F64F64ToF64Scalar {
    type State = Arc<dyn ScalarFunctionDef>;
    fn invoke(
        state: &Self::State,
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let n = input.len();
        let v0 = input.flat_vector(0);
        let v1 = input.flat_vector(1);
        let r0 = unsafe { v0.as_slice_with_len::<f64>(n) };
        let r1 = unsafe { v1.as_slice_with_len::<f64>(n) };
        let mut out = output.flat_vector();
        let mut nulls: Vec<usize> = Vec::new();
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<f64>(n) };
            for i in 0..n {
                if propagates_null && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64)) { nulls.push(i); continue; }
                let r = state.execute(&[
                    FunctionValue::Float64(r0[i]),
                    FunctionValue::Float64(r1[i]),
                ]).map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Float64(v) => v,
                    FunctionValue::Float32(v) => v as f64,
                    FunctionValue::Int64(v) => v as f64,
                    FunctionValue::Int32(v) => v as f64,
                    FunctionValue::Null      => { nulls.push(i); 0.0 },
                    other => return Err(format!(
                        "F64F64ToF64Scalar: unexpected variant `{}`",
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
                LogicalTypeHandle::from(LogicalTypeId::Double),
                LogicalTypeHandle::from(LogicalTypeId::Double),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Double),
        )]
    }
}

// ---- (float64, float64, boolean, boolean) -> binary ----
fn register_f64_f64_bool_bool_to_blob(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<F64F64BoolBoolToBlobScalar>(sql_name, &def)
}
struct F64F64BoolBoolToBlobScalar;
impl VScalar for F64F64BoolBoolToBlobScalar {
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
        let v3 = input.flat_vector(3);
        let r0 = unsafe { v0.as_slice_with_len::<f64>(n) };
        let r1 = unsafe { v1.as_slice_with_len::<f64>(n) };
        let r2 = unsafe { v2.as_slice_with_len::<bool>(n) };
        let r3 = unsafe { v3.as_slice_with_len::<bool>(n) };
        let mut out = output.flat_vector();
        let propagates_null = state.propagates_null();
        for i in 0..n {
            if propagates_null && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64) || v2.row_is_null(i as u64) || v3.row_is_null(i as u64)) { out.set_null(i); continue; }
            let r = state.execute(&[
                FunctionValue::Float64(r0[i]),
                FunctionValue::Float64(r1[i]),
                FunctionValue::Boolean(r2[i]),
                FunctionValue::Boolean(r3[i]),
            ]).map_err(|e| format!("{e:?}"))?;
            insert_blob_result::<Self>(&mut out, i, r)?;
        }
        Ok(())
    }
    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![
                LogicalTypeHandle::from(LogicalTypeId::Double),
                LogicalTypeHandle::from(LogicalTypeId::Double),
                LogicalTypeHandle::from(LogicalTypeId::Boolean),
                LogicalTypeHandle::from(LogicalTypeId::Boolean),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Blob),
        )]
    }
}

// ---- (float64, float64, float64, float64) -> binary ----
fn register_f64_f64_f64_f64_to_blob(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<F64F64F64F64ToBlobScalar>(sql_name, &def)
}
struct F64F64F64F64ToBlobScalar;
impl VScalar for F64F64F64F64ToBlobScalar {
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
        let v3 = input.flat_vector(3);
        let r0 = unsafe { v0.as_slice_with_len::<f64>(n) };
        let r1 = unsafe { v1.as_slice_with_len::<f64>(n) };
        let r2 = unsafe { v2.as_slice_with_len::<f64>(n) };
        let r3 = unsafe { v3.as_slice_with_len::<f64>(n) };
        let mut out = output.flat_vector();
        let propagates_null = state.propagates_null();
        for i in 0..n {
            if propagates_null && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64) || v2.row_is_null(i as u64) || v3.row_is_null(i as u64)) { out.set_null(i); continue; }
            let r = state.execute(&[
                FunctionValue::Float64(r0[i]),
                FunctionValue::Float64(r1[i]),
                FunctionValue::Float64(r2[i]),
                FunctionValue::Float64(r3[i]),
            ]).map_err(|e| format!("{e:?}"))?;
            insert_blob_result::<Self>(&mut out, i, r)?;
        }
        Ok(())
    }
    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![
                LogicalTypeHandle::from(LogicalTypeId::Double),
                LogicalTypeHandle::from(LogicalTypeId::Double),
                LogicalTypeHandle::from(LogicalTypeId::Double),
                LogicalTypeHandle::from(LogicalTypeId::Double),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Blob),
        )]
    }
}

// ---- (float64, float64, float64, float64) -> float64 ----
fn register_f64_f64_f64_f64_to_f64(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<F64F64F64F64ToF64Scalar>(sql_name, &def)
}
struct F64F64F64F64ToF64Scalar;
impl VScalar for F64F64F64F64ToF64Scalar {
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
        let v3 = input.flat_vector(3);
        let r0 = unsafe { v0.as_slice_with_len::<f64>(n) };
        let r1 = unsafe { v1.as_slice_with_len::<f64>(n) };
        let r2 = unsafe { v2.as_slice_with_len::<f64>(n) };
        let r3 = unsafe { v3.as_slice_with_len::<f64>(n) };
        let mut out = output.flat_vector();
        let mut nulls: Vec<usize> = Vec::new();
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<f64>(n) };
            for i in 0..n {
                if propagates_null && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64) || v2.row_is_null(i as u64) || v3.row_is_null(i as u64)) { nulls.push(i); continue; }
                let r = state.execute(&[
                    FunctionValue::Float64(r0[i]),
                    FunctionValue::Float64(r1[i]),
                    FunctionValue::Float64(r2[i]),
                    FunctionValue::Float64(r3[i]),
                ]).map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Float64(v) => v,
                    FunctionValue::Float32(v) => v as f64,
                    FunctionValue::Int64(v) => v as f64,
                    FunctionValue::Int32(v) => v as f64,
                    FunctionValue::Null      => { nulls.push(i); 0.0 },
                    other => return Err(format!(
                        "F64F64F64F64ToF64Scalar: unexpected variant `{}`",
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
                LogicalTypeHandle::from(LogicalTypeId::Double),
                LogicalTypeHandle::from(LogicalTypeId::Double),
                LogicalTypeHandle::from(LogicalTypeId::Double),
                LogicalTypeHandle::from(LogicalTypeId::Double),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Double),
        )]
    }
}

// ---- (int64, float64) -> binary ----
fn register_i64_f64_to_blob(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<I64F64ToBlobScalar>(sql_name, &def)
}
struct I64F64ToBlobScalar;
impl VScalar for I64F64ToBlobScalar {
    type State = Arc<dyn ScalarFunctionDef>;
    fn invoke(
        state: &Self::State,
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let n = input.len();
        let v0 = input.flat_vector(0);
        let v1 = input.flat_vector(1);
        let r0 = unsafe { v0.as_slice_with_len::<i64>(n) };
        let r1 = unsafe { v1.as_slice_with_len::<f64>(n) };
        let mut out = output.flat_vector();
        let propagates_null = state.propagates_null();
        for i in 0..n {
            if propagates_null && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64)) { out.set_null(i); continue; }
            let r = state.execute(&[
                FunctionValue::Int64(r0[i]),
                FunctionValue::Float64(r1[i]),
            ]).map_err(|e| format!("{e:?}"))?;
            insert_blob_result::<Self>(&mut out, i, r)?;
        }
        Ok(())
    }
    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![
                LogicalTypeHandle::from(LogicalTypeId::Bigint),
                LogicalTypeHandle::from(LogicalTypeId::Double),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Blob),
        )]
    }
}

// ---- (int64, float64, float64) -> binary ----
fn register_i64_f64_f64_to_blob(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<I64F64F64ToBlobScalar>(sql_name, &def)
}
struct I64F64F64ToBlobScalar;
impl VScalar for I64F64F64ToBlobScalar {
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
        let r0 = unsafe { v0.as_slice_with_len::<i64>(n) };
        let r1 = unsafe { v1.as_slice_with_len::<f64>(n) };
        let r2 = unsafe { v2.as_slice_with_len::<f64>(n) };
        let mut out = output.flat_vector();
        let propagates_null = state.propagates_null();
        for i in 0..n {
            if propagates_null && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64) || v2.row_is_null(i as u64)) { out.set_null(i); continue; }
            let r = state.execute(&[
                FunctionValue::Int64(r0[i]),
                FunctionValue::Float64(r1[i]),
                FunctionValue::Float64(r2[i]),
            ]).map_err(|e| format!("{e:?}"))?;
            insert_blob_result::<Self>(&mut out, i, r)?;
        }
        Ok(())
    }
    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![
                LogicalTypeHandle::from(LogicalTypeId::Bigint),
                LogicalTypeHandle::from(LogicalTypeId::Double),
                LogicalTypeHandle::from(LogicalTypeId::Double),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Blob),
        )]
    }
}

// ---- (text, int64, int64) -> binary ----
fn register_text_i64_i64_to_blob(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<TextI64I64ToBlobScalar>(sql_name, &def)
}
struct TextI64I64ToBlobScalar;
impl VScalar for TextI64I64ToBlobScalar {
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
        let r1 = unsafe { v1.as_slice_with_len::<i64>(n) };
        let r2 = unsafe { v2.as_slice_with_len::<i64>(n) };
        let mut out = output.flat_vector();
        let propagates_null = state.propagates_null();
        for i in 0..n {
            if propagates_null && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64) || v2.row_is_null(i as u64)) { out.set_null(i); continue; }
            let mut s_0 = r0[i];
            let t_0: String = DuckString::new(&mut s_0).as_str().to_string();
            let r = state.execute(&[
                FunctionValue::String(t_0),
                FunctionValue::Int64(r1[i]),
                FunctionValue::Int64(r2[i]),
            ]).map_err(|e| format!("{e:?}"))?;
            insert_blob_result::<Self>(&mut out, i, r)?;
        }
        Ok(())
    }
    fn signatures() -> Vec<ScalarFunctionSignature> {
        vec![ScalarFunctionSignature::exact(
            vec![
                LogicalTypeHandle::from(LogicalTypeId::Varchar),
                LogicalTypeHandle::from(LogicalTypeId::Bigint),
                LogicalTypeHandle::from(LogicalTypeId::Bigint),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Blob),
        )]
    }
}

// ---- (text, int64, int64) -> float64 ----
fn register_text_i64_i64_to_f64(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<TextI64I64ToF64Scalar>(sql_name, &def)
}
struct TextI64I64ToF64Scalar;
impl VScalar for TextI64I64ToF64Scalar {
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
        let r1 = unsafe { v1.as_slice_with_len::<i64>(n) };
        let r2 = unsafe { v2.as_slice_with_len::<i64>(n) };
        let mut out = output.flat_vector();
        let mut nulls: Vec<usize> = Vec::new();
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<f64>(n) };
            for i in 0..n {
                if propagates_null && (v0.row_is_null(i as u64) || v1.row_is_null(i as u64) || v2.row_is_null(i as u64)) { nulls.push(i); continue; }
                let mut s_0 = r0[i];
                let t_0: String = DuckString::new(&mut s_0).as_str().to_string();
                let r = state.execute(&[
                    FunctionValue::String(t_0),
                    FunctionValue::Int64(r1[i]),
                    FunctionValue::Int64(r2[i]),
                ]).map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Float64(v) => v,
                    FunctionValue::Float32(v) => v as f64,
                    FunctionValue::Int64(v) => v as f64,
                    FunctionValue::Int32(v) => v as f64,
                    FunctionValue::Null      => { nulls.push(i); 0.0 },
                    other => return Err(format!(
                        "TextI64I64ToF64Scalar: unexpected variant `{}`",
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
                LogicalTypeHandle::from(LogicalTypeId::Varchar),
                LogicalTypeHandle::from(LogicalTypeId::Bigint),
                LogicalTypeHandle::from(LogicalTypeId::Bigint),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Double),
        )]
    }
}

// ---- (uint64) -> int64 ----
fn register_u64_to_i64(conn: &Connection, sql_name: &str) -> Result<()> {
    let def = lookup(sql_name)?;
    conn.register_scalar_function_with_state::<U64ToI64Scalar>(sql_name, &def)
}
struct U64ToI64Scalar;
impl VScalar for U64ToI64Scalar {
    type State = Arc<dyn ScalarFunctionDef>;
    fn invoke(
        state: &Self::State,
        input: &mut DataChunkHandle,
        output: &mut dyn WritableVector,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let n = input.len();
        let v0 = input.flat_vector(0);
        let r0 = unsafe { v0.as_slice_with_len::<u64>(n) };
        let mut out = output.flat_vector();
        let mut nulls: Vec<usize> = Vec::new();
        let propagates_null = state.propagates_null();
        {
            let out_slice = unsafe { out.as_mut_slice_with_len::<i64>(n) };
            for i in 0..n {
                if propagates_null && (v0.row_is_null(i as u64)) { nulls.push(i); continue; }
                let r = state.execute(&[
                    FunctionValue::UInt64(r0[i]),
                ]).map_err(|e| format!("{e:?}"))?;
                out_slice[i] = match r {
                    FunctionValue::Int64(v) => v,
                    FunctionValue::Int32(v) => v as i64,
                    FunctionValue::UInt32(v) => v as i64,
                    FunctionValue::UInt64(v) => v as i64,
                    FunctionValue::Null      => { nulls.push(i); 0 },
                    other => return Err(format!(
                        "U64ToI64Scalar: unexpected variant `{}`",
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
                LogicalTypeHandle::from(LogicalTypeId::UBigint),
            ],
            LogicalTypeHandle::from(LogicalTypeId::Bigint),
        )]
    }
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
    aggregates: RwLock<HashMap<String, Arc<dyn AggregateFunctionDef>>>,
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

    let mut capture = CapturingTarget {{
        scalars: Vec::new(),
        aggregates: Vec::new(),
    }};
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
    let mut aggregates = HashMap::with_capacity(capture.aggregates.len() * 2);
    for def in capture.aggregates {{
        let canonical = def.name().to_string();
        for alias in def.aliases() {{
            aggregates.insert(alias.to_string(), Arc::clone(&def));
        }}
        aggregates.insert(canonical, def);
    }}

    SHIM.set(ShimRegistry {{
        _ext: ext,
        scalars: RwLock::new(scalars),
        aggregates: RwLock::new(aggregates),
    }}).map_err(|_| anyhow::anyhow!("ShimRegistry already initialised"))?;

    Ok(())
}}

pub fn lookup_scalar(name: &str) -> Option<Arc<dyn ScalarFunctionDef>> {{
    let r = SHIM.get()?;
    r.scalars.read().get(name).cloned()
}}

pub fn lookup_aggregate(name: &str) -> Option<Arc<dyn AggregateFunctionDef>> {{
    let r = SHIM.get()?;
    r.aggregates.read().get(name).cloned()
}}

/// ExtensionTarget that captures every scalar and aggregate the
/// shim registers. UDTFs / windows / types / etc. are accepted
/// as no-ops until later phases.
struct CapturingTarget {{
    scalars: Vec<Arc<dyn ScalarFunctionDef>>,
    aggregates: Vec<Arc<dyn AggregateFunctionDef>>,
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
        def: Arc<dyn AggregateFunctionDef>,
    ) -> std::result::Result<(), ExtensionError> {{
        self.aggregates.push(def);
        Ok(())
    }}
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
r##"//! Aggregate-function registration via raw libduckdb-sys.
//!
//! Phase 3c (2026-06-24, unblocked once we forked duckdb-rs to
//! expose `Connection::handle()`): walks every shim aggregate
//! and wires it into DuckDB through the C API directly.
//!
//! Architecture
//!
//!   Per-aggregate-registration `ExtraInfo` allocation holds
//!   `Arc<dyn AggregateFunctionDef>`. The pointer is stashed on
//!   the duckdb_aggregate_function via
//!   `duckdb_aggregate_function_set_extra_info` and recovered
//!   in every callback via `duckdb_aggregate_function_get_extra_info`.
//!
//!   Per-group state (DuckDB allocates `state_size` bytes per
//!   group) is exactly 8 bytes: the thin pointer to a heap
//!   `Box<Box<dyn Accumulator>>`. `state_init` mints one;
//!   `state_destroy` drops it.
//!
//!   `update` walks the input chunk row-by-row, recovering the
//!   per-group accumulator via the thin pointer; `combine`
//!   merges source into target; `finalize` produces the result
//!   and writes it to the output vector slot.

use std::ffi::CString;
use std::os::raw::{c_char, c_void};
use std::sync::Arc;

use libduckdb_sys::{
    self as ffi, DuckDBSuccess, duckdb_aggregate_function_add_parameter,
    duckdb_aggregate_function_get_extra_info, duckdb_aggregate_function_set_error,
    duckdb_aggregate_function_set_extra_info, duckdb_aggregate_function_set_functions,
    duckdb_aggregate_function_set_name, duckdb_aggregate_function_set_return_type,
    duckdb_aggregate_state, duckdb_connection, duckdb_create_aggregate_function,
    duckdb_data_chunk, duckdb_destroy_aggregate_function, duckdb_function_info,
    duckdb_register_aggregate_function, duckdb_string_t, duckdb_vector, idx_t,
};

use datafission_functions::traits::{Accumulator, AggregateFunctionDef};
use datafission_functions::types::FunctionValue;

use crate::registry;

/// Per-registration state. Stashed on the aggregate via
/// duckdb_aggregate_function_set_extra_info; recovered in every
/// callback.
struct AggExtraInfo {
    def: Arc<dyn AggregateFunctionDef>,
}

/// Register every aggregate the shim publishes.
///
/// # Safety
///
/// `conn` must be a valid `duckdb_connection` for the duration
/// of this call.
pub unsafe fn register_all(conn: duckdb_connection) {
"##,
    );

    let mut canonical = 0usize;
    let mut alias_count = 0usize;
    for ext in &plan.extensions {
        for agg in &ext.aggregates {
            s.push_str(&format!(
                "    register_aggregate(conn, \"{name}\");\n",
                name = agg.canonical_name,
            ));
            for alias in &agg.aliases {
                s.push_str(&format!(
                    "    register_aggregate(conn, \"{alias}\"); // alias of {name}\n",
                    alias = alias, name = agg.canonical_name,
                ));
                alias_count += 1;
            }
            canonical += 1;
        }
    }
    s.push_str(&format!(
        "    // Phase 3c: {canonical} canonical + {alias_count} alias names registered.\n"
    ));
    if canonical == 0 {
        s.push_str("    // (no aggregates in this interface DB)\n");
    }

    s.push_str(
r##"}

unsafe fn register_aggregate(conn: duckdb_connection, sql_name: &str) {
    let def = match registry::lookup_aggregate(sql_name) {
        Some(d) => d,
        None => {
            eprintln!("[shim-aggregates] no shim entry for `{sql_name}` — skipping");
            return;
        }
    };

    let agg = duckdb_create_aggregate_function();
    let name_cs = match CString::new(sql_name) {
        Ok(c) => c,
        Err(_) => {
            eprintln!("[shim-aggregates] name contains NUL: `{sql_name}` — skipping");
            duckdb_destroy_aggregate_function(&mut { agg });
            return;
        }
    };
    duckdb_aggregate_function_set_name(agg, name_cs.as_ptr());

    // PostGIS aggregates are unary (ST_Union, ST_Extent,
    // ST_Collect — all take one geometry blob). For multi-arg
    // future aggregates, walk def.param_types() and call
    // add_parameter per type. Today's flat BLOB signature is
    // shape-correct for every shim aggregate observed.
    let blob = ffi::duckdb_create_logical_type(ffi::DUCKDB_TYPE_DUCKDB_TYPE_BLOB);
    duckdb_aggregate_function_add_parameter(agg, blob);
    duckdb_aggregate_function_set_return_type(agg, blob);
    // Both add_parameter and set_return_type take the type by
    // value and DuckDB clones it internally, so we still own
    // `blob` and have to destroy it after use.
    let mut blob_to_destroy = blob;
    ffi::duckdb_destroy_logical_type(&mut blob_to_destroy);

        // Wire the callbacks (state_size + state_init + update +
        // combine + finalize). state_destroy goes through the
        // separate set_destructor API — see drop_state below.
        duckdb_aggregate_function_set_functions(
            agg,
            Some(state_size),
            Some(state_init),
            Some(update_callback),
            Some(combine_callback),
            Some(finalize_callback),
        );
    ffi::duckdb_aggregate_function_set_destructor(agg, Some(state_destroy));

    // Park the per-registration state. The Box is leaked here
    // and reclaimed when DuckDB calls the destructor.
    let extra = Box::into_raw(Box::new(AggExtraInfo { def: Arc::clone(&def) }));
    duckdb_aggregate_function_set_extra_info(
        agg,
        extra as *mut c_void,
        Some(drop_extra_info),
    );

    let rc = duckdb_register_aggregate_function(conn, agg);
    duckdb_destroy_aggregate_function(&mut { agg });
    if rc != DuckDBSuccess {
        // 5 PostGIS aggregate names clash with scalar variants
        // (st_collect, st_union, st_clusterwithin,
        // st_clusterdbscan, st_clusterintersecting). DuckDB
        // rejects the registration in that case. Swallow the
        // error so the non-clashing aggregates still register —
        // the clashing ones fall back to the already-registered
        // scalar form, which has the semantics the user probably
        // wanted anyway.
        eprintln!(
            "postgis-duckdb-bridge: skipping aggregate `{sql_name}` \
             (name already in use by a scalar; rc={rc})"
        );
    }
}

// =====================================================================
// C callbacks. All `unsafe extern "C"` because DuckDB calls them.
//
// State layout: each per-group state is exactly 8 bytes — a
// thin pointer to a heap-allocated `Box<dyn Accumulator>`.
// `state_init` mints one; `state_destroy` drops it. We use
// Box<dyn Accumulator> directly (fat ptr → 16 bytes) but stored
// behind one more Box layer so the THIN pointer is what lives
// in the state slot.
// =====================================================================

type AccBoxPtr = *mut Box<dyn Accumulator>;

unsafe extern "C" fn state_size(_info: duckdb_function_info) -> idx_t {
    std::mem::size_of::<AccBoxPtr>() as idx_t
}

unsafe extern "C" fn state_init(info: duckdb_function_info, state: duckdb_aggregate_state) {
    let extra = extra_info_typed(info);
    let acc: Box<dyn Accumulator> = extra.def.create_accumulator();
    let thin: AccBoxPtr = Box::into_raw(Box::new(acc));
    // The state pointer points to STATE_SIZE bytes that DuckDB
    // owns. Treat it as a slot for our thin pointer.
    let slot = state as *mut AccBoxPtr;
    std::ptr::write(slot, thin);
}

unsafe extern "C" fn state_destroy(states: *mut duckdb_aggregate_state, count: idx_t) {
    if states.is_null() { return; }
    for i in 0..count as usize {
        let state_ptr: duckdb_aggregate_state = std::ptr::read(states.add(i));
        if state_ptr.is_null() { continue; }
        let slot = state_ptr as *mut AccBoxPtr;
        let thin = std::ptr::read(slot);
        if !thin.is_null() {
            drop(Box::from_raw(thin));
        }
    }
}

unsafe extern "C" fn update_callback(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    states: *mut duckdb_aggregate_state,
) {
    match std::panic::catch_unwind(|| update_inner(info, input, states)) {
        Ok(Ok(())) => {}
        Ok(Err(msg)) => set_error(info, &msg),
        Err(_)      => set_error(info, "panic in aggregate update"),
    }
}

unsafe fn update_inner(
    _info: duckdb_function_info,
    input: duckdb_data_chunk,
    states: *mut duckdb_aggregate_state,
) -> std::result::Result<(), String> {
    let n = ffi::duckdb_data_chunk_get_size(input) as usize;
    let v0 = ffi::duckdb_data_chunk_get_vector(input, 0);
    let data = ffi::duckdb_vector_get_data(v0) as *const duckdb_string_t;
    let validity = ffi::duckdb_vector_get_validity(v0);

    for i in 0..n {
        if !validity.is_null() && !ffi::duckdb_validity_row_is_valid(validity, i as idx_t) {
            // SQL aggregates skip NULLs unless they're COUNT(*) —
            // PostGIS aggregates all skip NULL.
            continue;
        }
        let mut s_raw: duckdb_string_t = std::ptr::read(data.add(i));
        let bytes = read_string_t_bytes(&mut s_raw);
        let acc_ptr = read_state(states, i);
        let acc = &mut *acc_ptr;
        acc.accumulate(&FunctionValue::Binary(bytes))
            .map_err(|e| format!("{e:?}"))?;
    }
    Ok(())
}

/// Decode a `duckdb_string_t` (DuckDB's inline-or-pointer
/// string struct) into an owned `Vec<u8>`. Works for both
/// inlined (≤12 bytes) and pointer-backed strings; the C API
/// hides the discriminant for us.
unsafe fn read_string_t_bytes(s: &mut duckdb_string_t) -> Vec<u8> {
    let len = ffi::duckdb_string_t_length(*s) as usize;
    if len == 0 {
        return Vec::new();
    }
    let p = ffi::duckdb_string_t_data(s as *mut duckdb_string_t);
    std::slice::from_raw_parts(p as *const u8, len).to_vec()
}

unsafe extern "C" fn combine_callback(
    info: duckdb_function_info,
    source: *mut duckdb_aggregate_state,
    target: *mut duckdb_aggregate_state,
    count: idx_t,
) {
    let result = std::panic::catch_unwind(|| {
        for i in 0..count as usize {
            let src = read_state(source, i);
            let tgt = read_state(target, i);
            if src.is_null() || tgt.is_null() { continue; }
            let src_acc: &dyn Accumulator = &**src;
            let tgt_acc = &mut **tgt;
            tgt_acc.merge(src_acc).map_err(|e| format!("{e:?}"))?;
        }
        Ok::<(), String>(())
    });
    match result {
        Ok(Ok(())) => {}
        Ok(Err(msg)) => set_error(info, &msg),
        Err(_)      => set_error(info, "panic in aggregate combine"),
    }
}

unsafe extern "C" fn finalize_callback(
    info: duckdb_function_info,
    source: *mut duckdb_aggregate_state,
    result: duckdb_vector,
    count: idx_t,
    offset: idx_t,
) {
    match std::panic::catch_unwind(|| finalize_inner(info, source, result, count, offset)) {
        Ok(Ok(())) => {}
        Ok(Err(msg)) => set_error(info, &msg),
        Err(_)      => set_error(info, "panic in aggregate finalize"),
    }
}

unsafe fn finalize_inner(
    _info: duckdb_function_info,
    source: *mut duckdb_aggregate_state,
    result: duckdb_vector,
    count: idx_t,
    offset: idx_t,
) -> std::result::Result<(), String> {
    for i in 0..count as usize {
        let dst = offset as usize + i;
        let acc_ptr = read_state(source, i);
        if acc_ptr.is_null() {
            vector_set_null(result, dst);
            continue;
        }
        let acc = &**acc_ptr;
        let value = acc.finalize().map_err(|e| format!("{e:?}"))?;
        match value {
            FunctionValue::Binary(b) => vector_assign_bytes(result, dst, &b),
            FunctionValue::String(s) => vector_assign_bytes(result, dst, s.as_bytes()),
            FunctionValue::Null => vector_set_null(result, dst),
            other => return Err(format!(
                "aggregate finalize: unexpected result variant `{}`",
                other.type_name()
            )),
        }
    }
    Ok(())
}

/// Write `bytes` into the `dst`-th slot of `result` (a BLOB or
/// VARCHAR output vector). DuckDB copies the bytes internally,
/// so the caller's `bytes` can be dropped after this call.
unsafe fn vector_assign_bytes(result: duckdb_vector, dst: usize, bytes: &[u8]) {
    ffi::duckdb_vector_assign_string_element_len(
        result,
        dst as idx_t,
        bytes.as_ptr() as *const c_char,
        bytes.len() as idx_t,
    );
}

/// Mark the `dst`-th slot of `result` as SQL NULL.
unsafe fn vector_set_null(result: duckdb_vector, dst: usize) {
    ffi::duckdb_vector_ensure_validity_writable(result);
    let validity = ffi::duckdb_vector_get_validity(result);
    if !validity.is_null() {
        ffi::duckdb_validity_set_row_invalid(validity, dst as idx_t);
    }
}

unsafe extern "C" fn drop_extra_info(ptr: *mut c_void) {
    if ptr.is_null() { return; }
    drop(Box::from_raw(ptr as *mut AggExtraInfo));
}

// =====================================================================
// State-slot accessors.
// =====================================================================

unsafe fn read_state(states: *mut duckdb_aggregate_state, idx: usize) -> AccBoxPtr {
    // `states[i]` is a duckdb_aggregate_state (a pointer to the
    // per-group state slot). Dereference to get the slot
    // address, then read our AccBoxPtr out of the slot.
    //
    // (Initial implementation skipped the dereference and
    // crashed DuckDB with SIGBUS — the C API docs say:
    //   "states – a pointer to states – Each element points to
    //    the state for the corresponding input row."
    //  i.e. it's an array of state pointers, not the states
    //  themselves laid out contiguously.)
    let state_ptr: duckdb_aggregate_state = std::ptr::read(states.add(idx));
    let slot = state_ptr as *mut AccBoxPtr;
    std::ptr::read(slot)
}

unsafe fn extra_info_typed<'a>(info: duckdb_function_info) -> &'a AggExtraInfo {
    let raw = duckdb_aggregate_function_get_extra_info(info);
    &*(raw as *const AggExtraInfo)
}

unsafe fn set_error(info: duckdb_function_info, msg: &str) {
    if let Ok(cs) = CString::new(msg) {
        duckdb_aggregate_function_set_error(info, cs.as_ptr() as *const c_char);
    }
}
"##,
    );
    s
}

pub fn table_functions_rs(plan: &BridgePlan) -> String {
    let mut s = generated_header();
    s.push_str(
r##"//! Table-function registration.
//!
//! ## Phase 4c — SCAFFOLDED (2026-06-24)
//!
//! Unlike aggregates (Phase 3c) and custom types (Phase 4b),
//! DuckDB's TableFunction API IS exposed in duckdb-rs via
//! the `VTab` trait. The `hello-ext` example
//! (`~/.cargo/registry/.../duckdb-1.x/examples/hello-ext/main.rs`)
//! shows the canonical pattern: bind/init/func callbacks +
//! `con.register_table_function::<HelloVTab>("hello")`.
//!
//! Architecture for shipping this
//!
//!   1. Define one concrete `ShimVTab` type with a BindData
//!      that holds Arc<dyn TableFunctionDef> + the parameter
//!      values from the SQL call.
//!
//!   2. `bind` reads SQL params via `bind.get_parameter(i)`,
//!      builds the output column schema from
//!      `def.output_schema(...)`.
//!
//!   3. `init` creates a per-query InitData holding
//!      `Box<dyn TableFunctionIterator>` from
//!      `def.execute(&function_values)`.
//!
//!   4. `func` calls `iter.next_row()` repeatedly to fill the
//!      output DataChunk; sets output length when chunk full
//!      or iter exhausted.
//!
//! Same blocker as Phase 3c/4b — the way duckdb-rs threads
//! state through bind/init/func means each shim UDTF needs
//! its own per-name VTab struct (or unsafe shared state).
//! With 7 PostGIS UDTFs and 2-3 ducks of unsafe code per
//! per-UDTF impl, this is ~400 LOC that warrants its own
//! session.

use duckdb::{Connection, Result};

/// Phase-4c no-op. The shim's UDTFs are captured in the
/// registry; the day someone writes the ShimVTab adapter,
/// this becomes a loop over registry::all_table_functions().
pub fn register_all(_conn: &Connection) -> Result<()> {
    Ok(())
}

// ----------------------------------------------------------------------
// UDTFs the shim publishes.
// ----------------------------------------------------------------------

"##,
    );
    for ext in &plan.extensions {
        s.push_str(&format!("// === extension: {} ===\n", ext.name));
        for tf in &ext.table_functions {
            let nargs = tf.param_signatures.first().map(|v| v.len()).unwrap_or(0);
            s.push_str(&format!("// udtf `{}` (arity={})\n", tf.canonical_name, nargs));
            if !tf.aliases.is_empty() {
                s.push_str(&format!("//   aliases: {}\n", tf.aliases.join(", ")));
            }
        }
    }
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
r##"//! Custom column types — alias BLOB as GEOMETRY, GEOGRAPHY, …
//!
//! Each shim type is registered with DuckDB as an alias for
//! `BLOB`. After registration `CREATE TABLE t (g GEOMETRY)`
//! shows `GEOMETRY` in the schema; storage and the function
//! ABI stay BLOB so existing `ScalarFunctionSignature::exact`
//! signatures keep working unchanged.
//!
//! For each registered type name we call:
//!
//!     handle = duckdb_create_logical_type(DUCKDB_TYPE_BLOB)
//!     duckdb_logical_type_set_alias(handle, "GEOMETRY")
//!     duckdb_register_logical_type(con, handle, NULL)
//!     duckdb_destroy_logical_type(handle)
//!
//! `duckdb_create_type_info` is reserved-for-future-use per
//! upstream comments — we pass NULL. On a name clash with an
//! existing type DuckDB returns DuckDBError; we log+continue
//! to match the policy aggregates_rs uses for scalar/aggregate
//! collisions.

use libduckdb_sys as ffi;
use std::ffi::CString;

/// Register every shim-published custom type as a BLOB alias.
///
/// # Safety
///
/// `con` must be a valid `duckdb_connection` for the duration
/// of this call.
pub unsafe fn register_all(con: ffi::duckdb_connection) {
    if con.is_null() {
        return;
    }
"##,
    );
    let mut names: Vec<String> = Vec::new();
    for ext in &plan.extensions {
        for ct in &ext.column_types {
            let n = ct.type_name.trim().to_string();
            if !n.is_empty() {
                names.push(n);
            }
        }
    }
    names.sort();
    names.dedup();
    for n in &names {
        s.push_str(&format!("    register_type(con, {:?});\n", n));
    }
    s.push_str(
r##"}

unsafe fn register_type(con: ffi::duckdb_connection, name: &str) {
    let handle = ffi::duckdb_create_logical_type(ffi::DUCKDB_TYPE_DUCKDB_TYPE_BLOB);
    if handle.is_null() {
        eprintln!("[shim-types] could not create logical type for {name}");
        return;
    }
    let c_name = match CString::new(name) {
        Ok(c) => c,
        Err(_) => {
            eprintln!("[shim-types] type name contains NUL: {name}");
            let mut h = handle;
            ffi::duckdb_destroy_logical_type(&mut h);
            return;
        }
    };
    ffi::duckdb_logical_type_set_alias(handle, c_name.as_ptr());
    let rc = ffi::duckdb_register_logical_type(con, handle, std::ptr::null_mut());
    if rc != ffi::DuckDBSuccess {
        eprintln!(
            "[shim-types] could not register type {name} (rc={rc}) — \
             likely a name clash with an existing type"
        );
    }
    let mut h = handle;
    ffi::duckdb_destroy_logical_type(&mut h);
}

// ----------------------------------------------------------------------
// Column types the shim publishes.
// ----------------------------------------------------------------------

"##,
    );
    for ext in &plan.extensions {
        s.push_str(&format!("// === extension: {} ===\n", ext.name));
        for ct in &ext.column_types {
            s.push_str(&format!(
                "// type_id={:5} name={:<24} size={:>4}  cast_from={:?}  cast_to={:?}\n",
                ct.type_id, ct.type_name, ct.storage_size, ct.cast_from, ct.cast_to
            ));
        }
    }
    s
}

pub fn operators_rs(plan: &BridgePlan) -> String {
    let mut s = generated_header();
    s.push_str(
r##"//! Operator handling.
//!
//! ## Phase 4d — same architectural blocker as sqlink
//! (2026-06-24)
//!
//! DuckDB's parser tokenises operators at compile time too.
//! Custom symbols like `&&&` or `<#>` are unrecognised tokens
//! — `SELECT g1 &&& g2` is a parse error before any function
//! dispatch could intervene. DuckDB DOES recognise some
//! symbols (`<<`, `>>`, etc.) and could in principle bind them
//! to scalar functions, but the registration API isn't exposed
//! from duckdb-rs (same Connection-internals problem).
//!
//! ## Where this work actually lives
//!
//! SQL-preprocessing concern, not extension concern. The
//! proper home is a separate `ducklink-preprocess` crate that:
//!
//!   - Parses SQL via sqlparser-rs (DuckDB dialect)
//!   - Rewrites operators: `g1 && g2` → `st_bboxintersects(g1, g2)`
//!   - Rewrites casts to user-types: same approach
//!   - Hands rewritten SQL to duckdb-rs's `prepare`/`execute`
//!
//! That crate can be generated from the same BridgePlan but
//! is a separate target.

use duckdb::{Connection, Result};

pub fn register_all(_conn: &Connection) -> Result<()> {
    Ok(())
}

// ----------------------------------------------------------------------
// Operators the shim advertises.
// ----------------------------------------------------------------------

"##,
    );
    for ext in &plan.extensions {
        s.push_str(&format!("// === extension: {} ===\n", ext.name));
        for op in &ext.operators {
            s.push_str(&format!(
                "// `{}` (lhs={:?}, rhs={:?})  →  {}\n",
                op.symbol, op.lhs_type_id, op.rhs_type_id, op.function_name
            ));
        }
    }
    s
}

pub fn casts_rs(plan: &BridgePlan) -> String {
    let mut s = generated_header();
    s.push_str(
r##"//! CAST(x AS T) — identity casts for shim-published alias types.
//!
//! ## Phase 4d (this pass) — BLOB <-> alias identity casts
//!
//! Because every shim-published type (GEOMETRY, GEOGRAPHY,
//! STBOX, ...) is registered as an alias of BLOB
//! (`types::register_all`), the wire format is identical;
//! casts between BLOB and any alias are bytewise no-ops. Without
//! registering them, DuckDB rejects every
//! `CREATE TABLE t (g GEOMETRY); INSERT INTO t VALUES
//! (ST_GeomFromText(...))` with "Unimplemented type for cast
//! (BLOB -> GEOMETRY)" since the function returns BLOB and the
//! column expects GEOMETRY.
//!
//! Per alias name N, we register two casts:
//!   - BLOB -> N   (implicit cast, allows INSERT INTO t (N) VALUES (blob_fn(...)))
//!   - N -> BLOB   (implicit cast, allows blob_fn(t.n_column))
//!
//! Both share a single identity callback: read each row's
//! string_t from the input vector, write the same bytes to the
//! output via `duckdb_vector_assign_string_element_len`. The
//! callback is reused across every registered cast.
//!
//! ## Out of scope for this pass — functional casts
//!
//! Casts like `CAST(text AS GEOMETRY) -> ST_GeomFromText(text)`
//! are source-shape driven (depends on what the source expression
//! IS, not just its type). DuckDB's cast hooks are type-driven,
//! so those rewrites belong in `shim-sql-preprocess` at the AST
//! layer, which already handles them.

use libduckdb_sys::{
    self as ffi, DuckDBSuccess, duckdb_function_info,
    duckdb_connection, duckdb_data_chunk_get_size,
    duckdb_string_t, duckdb_vector, duckdb_vector_get_data,
    duckdb_vector_get_validity, duckdb_validity_row_is_valid,
    duckdb_validity_set_row_invalid, duckdb_vector_ensure_validity_writable,
    duckdb_vector_assign_string_element_len, idx_t,
};
use std::ffi::CString;
use std::os::raw::c_char;

/// Register every BLOB <-> alias identity cast the shim
/// publishes via `column_types`.
///
/// # Safety
///
/// `con` must be a valid `duckdb_connection` for the duration
/// of this call.
pub unsafe fn register_all(con: duckdb_connection) {
    if con.is_null() {
        return;
    }
"##,
    );
    let mut names: Vec<String> = Vec::new();
    for ext in &plan.extensions {
        for ct in &ext.column_types {
            let n = ct.type_name.trim().to_string();
            if !n.is_empty() {
                names.push(n);
            }
        }
    }
    names.sort();
    names.dedup();
    for n in &names {
        s.push_str(&format!("    register_identity_cast(con, {:?});\n", n));
    }
    s.push_str(
r##"}

unsafe fn register_identity_cast(con: duckdb_connection, alias: &str) {
    let blob = ffi::duckdb_create_logical_type(ffi::DUCKDB_TYPE_DUCKDB_TYPE_BLOB);
    let aliased = ffi::duckdb_create_logical_type(ffi::DUCKDB_TYPE_DUCKDB_TYPE_BLOB);
    if blob.is_null() || aliased.is_null() {
        eprintln!("[shim-casts] could not create logical type for {alias}");
        return;
    }
    let c_alias = match CString::new(alias) {
        Ok(c) => c,
        Err(_) => {
            eprintln!("[shim-casts] alias name contains NUL: {alias}");
            let (mut a, mut b) = (aliased, blob);
            ffi::duckdb_destroy_logical_type(&mut a);
            ffi::duckdb_destroy_logical_type(&mut b);
            return;
        }
    };
    ffi::duckdb_logical_type_set_alias(aliased, c_alias.as_ptr());

    // BLOB -> alias
    register_one_direction(con, blob, aliased, alias, "blob_to_alias");
    // alias -> BLOB
    register_one_direction(con, aliased, blob, alias, "alias_to_blob");

    let (mut a, mut b) = (aliased, blob);
    ffi::duckdb_destroy_logical_type(&mut a);
    ffi::duckdb_destroy_logical_type(&mut b);
}

unsafe fn register_one_direction(
    con: duckdb_connection,
    source: ffi::duckdb_logical_type,
    target: ffi::duckdb_logical_type,
    alias: &str,
    direction: &str,
) {
    let cast = ffi::duckdb_create_cast_function();
    ffi::duckdb_cast_function_set_source_type(cast, source);
    ffi::duckdb_cast_function_set_target_type(cast, target);
    // Cost of 1 — explicit casts always free; implicit casts
    // prefer this over more-expensive paths but DuckDB's own
    // built-ins (cost 0) still win where they apply.
    ffi::duckdb_cast_function_set_implicit_cast_cost(cast, 1);
    ffi::duckdb_cast_function_set_function(cast, Some(identity_cast_callback));

    let rc = ffi::duckdb_register_cast_function(con, cast);
    let mut c = cast;
    ffi::duckdb_destroy_cast_function(&mut c);
    if rc != DuckDBSuccess {
        eprintln!(
            "[shim-casts] {direction} for {alias} failed (rc={rc}) — \
             may already be registered or a built-in handles it"
        );
    }
}

/// Per-row identity copy: every BLOB-shaped value is bytewise
/// identical between the source and target representations
/// because both sides are LogicalTypeId::Blob. We just memcpy
/// each row from the input vector to the output.
///
/// # Safety
///
/// Called by DuckDB. `input` / `output` are valid
/// `duckdb_vector` handles for the duration of the call.
unsafe extern "C" fn identity_cast_callback(
    _info: duckdb_function_info,
    count: idx_t,
    input: duckdb_vector,
    output: duckdb_vector,
) -> bool {
    let n = count as usize;
    if n == 0 {
        return true;
    }
    let in_data = duckdb_vector_get_data(input) as *const duckdb_string_t;
    let in_validity = duckdb_vector_get_validity(input);

    for i in 0..n {
        if !in_validity.is_null() && !duckdb_validity_row_is_valid(in_validity, i as idx_t) {
            duckdb_vector_ensure_validity_writable(output);
            let out_validity = duckdb_vector_get_validity(output);
            if !out_validity.is_null() {
                duckdb_validity_set_row_invalid(out_validity, i as idx_t);
            }
            continue;
        }
        let mut s_raw: duckdb_string_t = std::ptr::read(in_data.add(i));
        let len = ffi::duckdb_string_t_length(s_raw) as usize;
        let p = ffi::duckdb_string_t_data(&mut s_raw as *mut duckdb_string_t);
        duckdb_vector_assign_string_element_len(
            output,
            i as idx_t,
            p as *const c_char,
            len as idx_t,
        );
    }

    // Touch `count` to silence the unused warning on early-return paths.
    let _ = duckdb_data_chunk_get_size;
    true
}

// ----------------------------------------------------------------------
// Cast rewrites the shim advertises (for future functional-cast pass).
// ----------------------------------------------------------------------

"##,
    );
    for ext in &plan.extensions {
        s.push_str(&format!("// === extension: {} ===\n", ext.name));
        for c in &ext.cast_rewrites {
            s.push_str(&format!(
                "// CAST(<{}> AS {}) → {} (hint: {})\n",
                c.source_kind, c.target_type, c.function_name, c.source_fn_hint
            ));
        }
    }
    s
}

pub fn preprocessors_rs(plan: &BridgePlan) -> String {
    let mut s = generated_header();
    s.push_str(
r##"//! Token-level preprocessor patterns.
//!
//! ## Phase 4d — see operators.rs for the architectural picture.

use duckdb::{Connection, Result};

pub fn register_all(_conn: &Connection) -> Result<()> {
    Ok(())
}

// ----------------------------------------------------------------------
// Preprocessor patterns the shim advertises.
// ----------------------------------------------------------------------

"##);
    for ext in &plan.extensions {
        s.push_str(&format!("// === extension: {} ===\n", ext.name));
        for p in &ext.preprocessor_patterns {
            s.push_str(&format!("// token `{}` → {}\n", p.op_token, p.function_name));
        }
    }
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
    let primary = primary_extension_name(plan);
    let crate_name = sanitize_crate_name(&primary);
    let shim_env = format!("{}_SHIM_WASM", primary.to_uppercase().replace('-', "_"));
    let lib_name = format!("lib{}_duckdb_bridge.dylib", crate_name.replace('-', "_"));
    let ext_name = format!("{crate_name}_duckdb_bridge.duckdb_extension");

    let mut s = String::new();
    s.push_str(&format!("# {primary}-duckdb-bridge\n\n"));
    s.push_str(&format!(
        "Generated DuckDB loadable extension that bridges the **{primary}** DataFission \
         wasm shim into DuckDB as native scalar functions, aggregates, UDTFs, custom \
         column types, and identity casts.\n\n"
    ));
    s.push_str("Produced by [`ducklink-shim-codegen`](https://github.com/zacharywhitley/ducklink-shim-codegen) \
                from a shim-interface SQLite database. **Do not edit by hand** — regenerate \
                from the source.\n\n");

    s.push_str("## Surface\n\n");
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

    s.push_str(&format!(
r##"
## Build

```sh
cargo build --release
```

The build needs sibling checkouts of the path-dep'd workspace
crates (`datafission-df-plugin-loader`, `datafission-df-plugin-api`,
`datafission-functions`) at `../datafission/crates/`.

## Package as a loadable extension

DuckDB only loads files ending in `.duckdb_extension` that carry
a metadata footer matching the target DuckDB version. The
[`extension-template-rs`](https://github.com/duckdb/extension-template-rs)
project ships a helper script for this:

```sh
python3 path/to/append_extension_metadata.py \
  -l target/release/{lib_name} \
  -n {crate_name}_duckdb_bridge \
  -p osx_arm64 \
  -dv v1.2.0 \
  -ev v0.1.0 \
  -o {ext_name} \
  --abi-type C_STRUCT
```

Adjust `-p` to your platform (`linux_amd64`, `linux_arm64`,
`osx_amd64`, …). Pin `-dv` to the DuckDB version you target.

## Load + use

The bridge needs the composed shim wasm at runtime; set
`{shim_env}` before `LOAD`:

```sql
-- Need DuckDB CLI with -unsigned since we don't sign the bridge.
{shim_env}=/path/to/{primary}-composed.wasm duckdb -unsigned
> LOAD '/path/to/{ext_name}';
```

## Regen

When the upstream shim's SQL surface changes:

```sh
cd ~/git/ducklink-shim-codegen
cargo run --release -- \
  --interface /path/to/{primary}-interface.sqlite \
  --out ~/git/{primary}-duckdb-bridge
```

The codegen pipes every emitted `.rs` through
`rustfmt --edition 2021`, so the resulting crate is
`cargo fmt -p {crate_name}-duckdb-bridge -- --check`-clean by
construction.

## Architecture

- Scalars: dispatched through duckdb-rs's `VScalar` trait
  (vectorised; one chunk per invoke).
- Aggregates + custom types + identity casts: raw
  `libduckdb-sys` FFI — duckdb-rs doesn't surface these in its
  safe wrapper.
- Entrypoint: hand-rolled `extern "C"` symbol named
  `{crate_name}_duckdb_bridge_init_c_api` (NOT the
  `#[duckdb_entrypoint_c_api]` macro) so we can pull both a raw
  `duckdb_connection` AND a `Connection` wrapper.
- Custom types like `GEOMETRY` / `STBOX` register as BLOB
  aliases; identity casts in both directions let
  `CREATE TABLE t (g GEOMETRY); INSERT INTO t VALUES
  (st_geomfromtext(...))` round-trip.

## License

Apache-2.0. Generated source so the same license as the
codegen.
"##,
    ));
    s
}

fn generated_header() -> String {
    // dead_code: scalars.rs emits one VScalar struct per
    // supported shape (~30 today). Each generated bridge crate
    // only invokes a subset of `register_*` helpers — the
    // others are dispatched dynamically per shape, never named
    // directly. Suppressing the per-file warning avoids 20+
    // noise items per build without forcing per-helper
    // conditional emission.
    "// === GENERATED by ducklink-shim-codegen — do not edit by hand ===\n\
     #![allow(dead_code, unused_imports)]\n\n".into()
}

fn sanitize_crate_name(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

fn primary_extension_name(plan: &BridgePlan) -> String {
    plan.extensions.first().map(|e| e.name.clone()).unwrap_or_else(|| "shim".into())
}

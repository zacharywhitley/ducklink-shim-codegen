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
# Spatial-index build + query. The shim load in `registry.rs`
# installs the wasm-backed SpatialIndexBuilder into this crate's
# process registry (RuntimeWasmExtension::register ->
# register_spatial_builder), so `build_spatial_index(name, items)`
# routes to the REAL shim R-tree / STRtree. Its async build/query
# surface is driven from the sync DuckDB callbacks via a
# current-thread tokio runtime.
datafission-index            = {{ path = "../datafission/crates/index" }}
tokio                        = {{ version = "1", features = ["rt"] }}

# Stock duckdb-rs from crates.io. We use it for the typed
# `Connection` + `VScalar` trait (scalar dispatch). The
# aggregate / custom-type registration paths go directly
# through `libduckdb-sys` raw FFI — duckdb-rs doesn't surface
# those APIs in its safe wrapper but the C ABI is fully
# accessible via libduckdb-sys (a re-export of libduckdb-sys's
# `loadable-extension` feature).
duckdb         = {{ version = "~1.10504", features = ["vscalar", "vtab", "loadable-extension"] }}
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

    // Step 6 — scalars via raw libduckdb-sys. Each scalar is
    // registered as a function SET (one entry per parameter-list
    // overload) through the C scalar-function API, with a single
    // GENERIC dispatcher that reads each argument by its declared
    // DataType and writes the return by its declared type. This
    // subsumes every (param-types -> return) shape the shim
    // publishes — no enumerated shape table, no BLOB fallback.
    scalars::register_all(raw_con);

    // Step 7 — aggregates via raw libduckdb-sys.
    aggregates::register_all(raw_con);

    // Step 8 — UDTFs via raw libduckdb-sys (DuckDB's VTab
    // bind/init hooks are static, so we dispatch dynamically by
    // name through the C table-function API — same pattern as
    // aggregates).
    table_functions::register_all(raw_con);

    // Step 9 — spatial indexes. The loadable C API can't register
    // a custom INDEX access method (no duckdb_register_index_type),
    // so `CREATE INDEX … USING <name>` remains unsupported (a
    // documented limitation). Instead the shim's tested
    // spatial-index build + query path is exposed as a build
    // AGGREGATE plus query TABLE FUNCTIONS — see spatial_indexes.rs.
    spatial_indexes::register_all(raw_con);

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
        r##"//! Scalar-function registration via raw libduckdb-sys.
//!
//! ## Generic dispatch (2026-06-25) — arbitrary shapes
//!
//! Every shim scalar is registered through DuckDB's C
//! scalar-function API as a function SET (one entry per
//! parameter-list overload). A SINGLE generic dispatcher serves
//! every scalar regardless of arity, parameter types, or return
//! type: it reads each argument by its declared `DataType` (from
//! the interface DB signature) into a `FunctionValue`, calls
//! `ScalarFunctionDef::execute`, and writes the return by its
//! declared `DataType`.
//!
//! This replaces the earlier enumerated (param-types,
//! return-type) shape table. There is NO BLOB fallback: a scalar
//! whose true signature is float64-first, int-first, mixed, or
//! arbitrary-arity now registers with its REAL parameter + return
//! types, so `duckdb_functions().parameter_types` reflects the
//! shim's actual ABI rather than collapsing everything to BLOB.
//!
//! ### Overloads
//!
//! Unlike the table-function C API, the scalar C API has a
//! function-SET abstraction (`duckdb_create_scalar_function_set`
//! + `duckdb_add_scalar_function_to_set`). Each positional
//! overload (e.g. `st_addface(a, b)` AND `st_addface(a, b, true)`)
//! is added to the set under one name, each carrying its own
//! parameter `DataType`s on its ExtraInfo so the dispatcher reads
//! every argument with the right getter.
//!
//! ### Scalar/aggregate name clash
//!
//! DuckDB's loadable C API forbids a scalar and an aggregate
//! sharing a name in the same catalog. The aggregate form is the
//! one users want for the clashing names (st_collect, st_union,
//! …), so this emit SKIPS any canonical name also published as an
//! aggregate, leaving the field clear for `aggregates.rs`. The set
//! is computed from the plan, so a future shim's clashes resolve
//! automatically.
//!
//! Dispatch goes through `registry::lookup_scalar(name)` ->
//! `Arc<dyn ScalarFunctionDef>` + `execute` (unchanged); only the
//! SIGNATURE registration + per-arg read / return write are
//! generic.

use std::ffi::{c_char, c_void, CString};
use std::sync::Arc;

use libduckdb_sys::{
    self as ffi, duckdb_add_scalar_function_to_set, duckdb_connection,
    duckdb_create_scalar_function, duckdb_create_scalar_function_set, duckdb_data_chunk,
    duckdb_data_chunk_get_size, duckdb_data_chunk_get_vector, duckdb_destroy_scalar_function,
    duckdb_destroy_scalar_function_set, duckdb_function_info, duckdb_register_scalar_function_set,
    duckdb_scalar_function_add_parameter, duckdb_scalar_function_get_extra_info,
    duckdb_scalar_function_set_error, duckdb_scalar_function_set_extra_info,
    duckdb_scalar_function_set_function, duckdb_scalar_function_set_name,
    duckdb_scalar_function_set_return_type, duckdb_string_t, duckdb_vector, idx_t, DuckDBSuccess,
};

use datafission_functions::traits::ScalarFunctionDef;
use datafission_functions::types::FunctionValue;
use datafission_functions::DataType;

use crate::registry;

/// Per-overload state stashed on each scalar-function entry via
/// `duckdb_scalar_function_set_extra_info`; recovered in `invoke`.
/// `param_types` is THIS overload's signature; `return_type` its
/// declared output type.
struct ScExtraInfo {
    def: Arc<dyn ScalarFunctionDef>,
    param_types: Vec<DataType>,
    return_type: DataType,
}

/// Register every scalar the shim publishes.
///
/// # Safety
///
/// `conn` must be a valid `duckdb_connection` for the duration of
/// this call.
pub unsafe fn register_all(conn: duckdb_connection) {
"##,
    );

    // Aggregate wins on a scalar/aggregate name clash (see module
    // docs). Computed from the plan so it's shim-agnostic.
    let aggregate_names: std::collections::HashSet<&str> = plan
        .extensions
        .iter()
        .flat_map(|e| e.aggregates.iter().map(|a| a.canonical_name.as_str()))
        .collect();

    let mut canonical = 0usize;
    let mut alias_count = 0usize;
    let mut overloads = 0usize;
    for ext in &plan.extensions {
        for sc in &ext.scalars {
            if aggregate_names.contains(sc.canonical_name.as_str()) {
                s.push_str(&format!(
                    "    // scalar `{}` skipped — name also published as an aggregate; \
                     the aggregate form is registered instead (see aggregates.rs).\n",
                    sc.canonical_name,
                ));
                continue;
            }
            // Build the &[(&[DataType], DataType)] overload list as
            // a Rust expression. Every overload registers — no shape
            // is dropped.
            let ret = type_name_to_datatype_expr(&sc.return_type);
            let mut sig_exprs: Vec<String> = Vec::new();
            for params in &sc.param_signatures {
                let pts = params
                    .iter()
                    .map(|t| type_name_to_datatype_expr(t))
                    .collect::<Vec<_>>()
                    .join(", ");
                sig_exprs.push(format!("(&[{pts}], {ret})"));
                overloads += 1;
            }
            if sig_exprs.is_empty() {
                // Defensive: a scalar with no recorded signature.
                // Register a zero-arg overload returning its type.
                sig_exprs.push(format!("(&[], {ret})"));
            }
            let sigs = sig_exprs.join(", ");
            s.push_str(&format!(
                "    register_scalar(conn, {name:?}, &[{sigs}]);\n",
                name = sc.canonical_name,
                sigs = sigs,
            ));
            for alias in &sc.aliases {
                s.push_str(&format!(
                    "    register_scalar(conn, {alias:?}, &[{sigs}]); // alias of {name}\n",
                    alias = alias,
                    name = sc.canonical_name,
                    sigs = sigs,
                ));
                alias_count += 1;
            }
            canonical += 1;
        }
    }
    s.push_str(&format!(
        "    // Generic dispatch: {canonical} canonical + {alias_count} alias scalars \
         ({overloads} parameter-list overloads), all registered with their REAL \
         param + return types.\n"
    ));
    if canonical == 0 {
        s.push_str("    // (no scalars in this interface DB)\n");
    }

    s.push_str(SCALARS_RUNTIME);
    s
}

/// The generic scalar runtime — the set registrar + the one C
/// callback. Emitted verbatim into every generated bridge so each
/// crate is self-contained.
const SCALARS_RUNTIME: &str = r##"}

/// Register one scalar (by canonical name or alias) as a function
/// SET, one entry per parameter-list overload, each driven by the
/// generic `invoke` dispatcher.
unsafe fn register_scalar(
    conn: duckdb_connection,
    sql_name: &str,
    overloads: &[(&[DataType], DataType)],
) {
    let def = match registry::lookup_scalar(sql_name) {
        Some(d) => d,
        None => {
            eprintln!("[shim-scalars] no shim entry for `{sql_name}` — skipping");
            return;
        }
    };

    let name_cs = match CString::new(sql_name) {
        Ok(c) => c,
        Err(_) => {
            eprintln!("[shim-scalars] name contains NUL: `{sql_name}` — skipping");
            return;
        }
    };

    let set = duckdb_create_scalar_function_set(name_cs.as_ptr());
    let mut added = 0usize;
    for (params, ret) in overloads {
        let func = duckdb_create_scalar_function();
        duckdb_scalar_function_set_name(func, name_cs.as_ptr());

        for dt in params.iter() {
            let lt = ffi::duckdb_create_logical_type(logical_type_id_for(dt));
            duckdb_scalar_function_add_parameter(func, lt);
            let mut to_destroy = lt;
            ffi::duckdb_destroy_logical_type(&mut to_destroy);
        }
        let ret_lt = ffi::duckdb_create_logical_type(logical_type_id_for(ret));
        duckdb_scalar_function_set_return_type(func, ret_lt);
        let mut ret_to_destroy = ret_lt;
        ffi::duckdb_destroy_logical_type(&mut ret_to_destroy);

        duckdb_scalar_function_set_function(func, Some(invoke_callback));

        let extra = Box::into_raw(Box::new(ScExtraInfo {
            def: Arc::clone(&def),
            param_types: params.to_vec(),
            return_type: ret.clone(),
        }));
        duckdb_scalar_function_set_extra_info(func, extra as *mut c_void, Some(drop_extra_info));

        let rc = duckdb_add_scalar_function_to_set(set, func);
        // The set takes a copy of the function; we destroy our handle.
        duckdb_destroy_scalar_function(&mut { func });
        if rc != DuckDBSuccess {
            // Reclaim the leaked extra-info for this overload — the
            // set never took ownership of it.
            drop(Box::from_raw(extra));
            eprintln!(
                "[shim-scalars] could not add overload (arity {}) to `{sql_name}` (rc={rc})",
                params.len()
            );
            continue;
        }
        added += 1;
    }

    if added == 0 {
        duckdb_destroy_scalar_function_set(&mut { set });
        eprintln!("[shim-scalars] `{sql_name}`: no overloads registered — skipping set");
        return;
    }

    let rc = duckdb_register_scalar_function_set(conn, set);
    duckdb_destroy_scalar_function_set(&mut { set });
    if rc != DuckDBSuccess {
        eprintln!("[shim-scalars] could not register `{sql_name}` (rc={rc})");
    }
}

/// Map a DataFission `DataType` to the DuckDB C `DUCKDB_TYPE_*`
/// id used for parameter declarations and the return type.
fn logical_type_id_for(dt: &DataType) -> ffi::duckdb_type {
    match dt {
        DataType::Boolean => ffi::DUCKDB_TYPE_DUCKDB_TYPE_BOOLEAN,
        DataType::Int8 => ffi::DUCKDB_TYPE_DUCKDB_TYPE_TINYINT,
        DataType::Int16 => ffi::DUCKDB_TYPE_DUCKDB_TYPE_SMALLINT,
        DataType::Int32 => ffi::DUCKDB_TYPE_DUCKDB_TYPE_INTEGER,
        DataType::Int64 => ffi::DUCKDB_TYPE_DUCKDB_TYPE_BIGINT,
        DataType::UInt8 => ffi::DUCKDB_TYPE_DUCKDB_TYPE_UTINYINT,
        DataType::UInt16 => ffi::DUCKDB_TYPE_DUCKDB_TYPE_USMALLINT,
        DataType::UInt32 => ffi::DUCKDB_TYPE_DUCKDB_TYPE_UINTEGER,
        DataType::UInt64 => ffi::DUCKDB_TYPE_DUCKDB_TYPE_UBIGINT,
        DataType::Float32 => ffi::DUCKDB_TYPE_DUCKDB_TYPE_FLOAT,
        DataType::Float64 => ffi::DUCKDB_TYPE_DUCKDB_TYPE_DOUBLE,
        DataType::Char { .. } | DataType::Varchar { .. } | DataType::Text => {
            ffi::DUCKDB_TYPE_DUCKDB_TYPE_VARCHAR
        }
        DataType::Binary => ffi::DUCKDB_TYPE_DUCKDB_TYPE_BLOB,
        // Byte-oriented fallback (BLOB round-trips any payload).
        _ => ffi::DUCKDB_TYPE_DUCKDB_TYPE_BLOB,
    }
}

// =====================================================================
// The one generic C callback. DuckDB hands a chunk of N input rows
// and an output vector; we read each row's args by their declared
// DataType, call execute, and write the result by the declared
// return DataType.
// =====================================================================

unsafe extern "C" fn invoke_callback(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| invoke_inner(info, input, output)))
    {
        Ok(Ok(())) => {}
        Ok(Err(msg)) => set_err(info, &msg),
        Err(_) => set_err(info, "panic in scalar invoke"),
    }
}

unsafe fn invoke_inner(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    output: duckdb_vector,
) -> std::result::Result<(), String> {
    let extra = &*(duckdb_scalar_function_get_extra_info(info) as *const ScExtraInfo);
    let def = &extra.def;
    let n = duckdb_data_chunk_get_size(input) as usize;
    let ncols = extra.param_types.len();
    let propagates_null = def.propagates_null();

    for row in 0..n {
        // NULL propagation: if any input is NULL and the def
        // propagates nulls, emit NULL without calling the shim.
        if propagates_null {
            let mut any_null = false;
            for c in 0..ncols {
                let vec = duckdb_data_chunk_get_vector(input, c as idx_t);
                if vector_row_is_null(vec, row) {
                    any_null = true;
                    break;
                }
            }
            if any_null {
                set_vector_null(output, row);
                continue;
            }
        }

        let mut args: Vec<FunctionValue> = Vec::with_capacity(ncols);
        for c in 0..ncols {
            let vec = duckdb_data_chunk_get_vector(input, c as idx_t);
            args.push(read_cell(vec, row, &extra.param_types[c]));
        }
        let r = def.execute(&args).map_err(|e| format!("{e:?}"))?;
        write_cell(output, row, &r, &extra.return_type)?;
    }
    Ok(())
}

/// Read one input vector cell into a FunctionValue, using the
/// declared `DataType` to pick the physical representation.
unsafe fn read_cell(vec: duckdb_vector, row: usize, dt: &DataType) -> FunctionValue {
    if vector_row_is_null(vec, row) {
        return FunctionValue::Null;
    }
    match dt {
        DataType::Boolean => FunctionValue::Boolean(read_prim::<bool>(vec, row)),
        DataType::Int8 => FunctionValue::Int8(read_prim::<i8>(vec, row)),
        DataType::Int16 => FunctionValue::Int16(read_prim::<i16>(vec, row)),
        DataType::Int32 => FunctionValue::Int32(read_prim::<i32>(vec, row)),
        DataType::Int64 => FunctionValue::Int64(read_prim::<i64>(vec, row)),
        DataType::UInt8 => FunctionValue::UInt8(read_prim::<u8>(vec, row)),
        DataType::UInt16 => FunctionValue::UInt16(read_prim::<u16>(vec, row)),
        DataType::UInt32 => FunctionValue::UInt32(read_prim::<u32>(vec, row)),
        DataType::UInt64 => FunctionValue::UInt64(read_prim::<u64>(vec, row)),
        DataType::Float32 => FunctionValue::Float32(read_prim::<f32>(vec, row)),
        DataType::Float64 => FunctionValue::Float64(read_prim::<f64>(vec, row)),
        DataType::Char { .. } | DataType::Varchar { .. } | DataType::Text => {
            FunctionValue::String(String::from_utf8_lossy(&read_string_bytes(vec, row)).into_owned())
        }
        // BLOB / fallback — raw bytes (WKB round-trips losslessly).
        _ => FunctionValue::Binary(read_string_bytes(vec, row)),
    }
}

/// Write a FunctionValue into the `row`-th slot of the output
/// vector, using the declared return `DataType`.
unsafe fn write_cell(
    vec: duckdb_vector,
    row: usize,
    val: &FunctionValue,
    out_ty: &DataType,
) -> std::result::Result<(), String> {
    if matches!(val, FunctionValue::Null) {
        set_vector_null(vec, row);
        return Ok(());
    }
    match out_ty {
        DataType::Boolean => write_prim::<bool>(vec, row, val.as_bool().unwrap_or(false)),
        DataType::Int8 => write_prim::<i8>(vec, row, val.as_i64().unwrap_or(0) as i8),
        DataType::Int16 => write_prim::<i16>(vec, row, val.as_i64().unwrap_or(0) as i16),
        DataType::Int32 => write_prim::<i32>(vec, row, val.as_i64().unwrap_or(0) as i32),
        DataType::Int64 => write_prim::<i64>(vec, row, val.as_i64().unwrap_or(0)),
        DataType::UInt8 => write_prim::<u8>(vec, row, val.as_i64().unwrap_or(0) as u8),
        DataType::UInt16 => write_prim::<u16>(vec, row, val.as_i64().unwrap_or(0) as u16),
        DataType::UInt32 => write_prim::<u32>(vec, row, val.as_i64().unwrap_or(0) as u32),
        DataType::UInt64 => write_prim::<u64>(vec, row, val.as_i64().unwrap_or(0) as u64),
        DataType::Float32 => write_prim::<f32>(vec, row, val.as_f64().unwrap_or(0.0) as f32),
        DataType::Float64 => write_prim::<f64>(vec, row, val.as_f64().unwrap_or(0.0)),
        DataType::Char { .. } | DataType::Varchar { .. } | DataType::Text => match val {
            FunctionValue::String(sv) => assign_bytes(vec, row, sv.as_bytes()),
            FunctionValue::Binary(b) => assign_bytes(vec, row, b),
            other => assign_bytes(vec, row, format!("{other:?}").as_bytes()),
        },
        // BLOB / fallback — raw bytes.
        _ => match val {
            FunctionValue::Binary(b) => assign_bytes(vec, row, b),
            FunctionValue::String(sv) => assign_bytes(vec, row, sv.as_bytes()),
            other => {
                return Err(format!(
                    "scalar return: unexpected variant `{}` for BLOB output",
                    other.type_name()
                ))
            }
        },
    }
    Ok(())
}

unsafe fn read_prim<T: Copy>(vec: duckdb_vector, row: usize) -> T {
    let data = ffi::duckdb_vector_get_data(vec) as *const T;
    std::ptr::read(data.add(row))
}

unsafe fn write_prim<T: Copy>(vec: duckdb_vector, row: usize, v: T) {
    let data = ffi::duckdb_vector_get_data(vec) as *mut T;
    std::ptr::write(data.add(row), v);
}

/// Read a `duckdb_string_t` cell (VARCHAR or BLOB share this
/// physical layout) into owned bytes, handling both the inline
/// short-string and the pointer long-string representations.
unsafe fn read_string_bytes(vec: duckdb_vector, row: usize) -> Vec<u8> {
    let data = ffi::duckdb_vector_get_data(vec) as *const duckdb_string_t;
    let s = &*data.add(row);
    let len = s.value.inlined.length as usize;
    // `length <= 12` => inlined bytes; else `ptr` points at the data.
    if len <= 12 {
        let inl = &s.value.inlined.inlined;
        let bytes = inl.as_ptr() as *const u8;
        std::slice::from_raw_parts(bytes, len).to_vec()
    } else {
        let p = s.value.pointer.ptr as *const u8;
        std::slice::from_raw_parts(p, len).to_vec()
    }
}

unsafe fn assign_bytes(vec: duckdb_vector, row: usize, bytes: &[u8]) {
    ffi::duckdb_vector_assign_string_element_len(
        vec,
        row as idx_t,
        bytes.as_ptr() as *const c_char,
        bytes.len() as idx_t,
    );
}

unsafe fn vector_row_is_null(vec: duckdb_vector, row: usize) -> bool {
    let validity = ffi::duckdb_vector_get_validity(vec);
    if validity.is_null() {
        return false;
    }
    !ffi::duckdb_validity_row_is_valid(validity, row as idx_t)
}

unsafe fn set_vector_null(vec: duckdb_vector, row: usize) {
    ffi::duckdb_vector_ensure_validity_writable(vec);
    let validity = ffi::duckdb_vector_get_validity(vec);
    if !validity.is_null() {
        ffi::duckdb_validity_set_row_invalid(validity, row as idx_t);
    }
}

unsafe extern "C" fn drop_extra_info(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    drop(Box::from_raw(ptr as *mut ScExtraInfo));
}

unsafe fn set_err(info: duckdb_function_info, msg: &str) {
    if let Ok(cs) = CString::new(msg) {
        duckdb_scalar_function_set_error(info, cs.as_ptr() as *const c_char);
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
    tables: RwLock<HashMap<String, Arc<dyn TableFunctionDef>>>,
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
        tables: Vec::new(),
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
    let mut tables = HashMap::with_capacity(capture.tables.len() * 2);
    for def in capture.tables {{
        let canonical = def.name().to_string();
        for alias in def.aliases() {{
            tables.insert(alias.to_string(), Arc::clone(&def));
        }}
        tables.insert(canonical, def);
    }}

    SHIM.set(ShimRegistry {{
        _ext: ext,
        scalars: RwLock::new(scalars),
        aggregates: RwLock::new(aggregates),
        tables: RwLock::new(tables),
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

pub fn lookup_table(name: &str) -> Option<Arc<dyn TableFunctionDef>> {{
    let r = SHIM.get()?;
    r.tables.read().get(name).cloned()
}}

/// ExtensionTarget that captures every scalar, aggregate, and
/// table function the shim registers. Windows / types / etc. are
/// accepted as no-ops until later phases.
struct CapturingTarget {{
    scalars: Vec<Arc<dyn ScalarFunctionDef>>,
    aggregates: Vec<Arc<dyn AggregateFunctionDef>>,
    tables: Vec<Arc<dyn TableFunctionDef>>,
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
        def: Arc<dyn TableFunctionDef>,
    ) -> std::result::Result<(), ExtensionError> {{
        self.tables.push(def);
        Ok(())
    }}
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
            // Read the recorded input type from the first param
            // signature; default to "binary" if absent. The shim's
            // F64 aggregates (tfloat_max_agg / tfloat_stddev_agg /
            // etc.) take a scalar f64, not a sequence blob; without
            // threading this through we'd register them as
            // (BLOB)->BLOB and the bridge would error at call time
            // with `accumulator/value type mismatch`.
            let in_ty = agg.param_signatures.first()
                .and_then(|sig| sig.first())
                .map(|t| t.as_str())
                .unwrap_or("binary");
            s.push_str(&format!(
                "    register_aggregate(conn, \"{name}\", {in_ty:?});\n",
                name = agg.canonical_name, in_ty = in_ty,
            ));
            for alias in &agg.aliases {
                s.push_str(&format!(
                    "    register_aggregate(conn, \"{alias}\", {in_ty:?}); // alias of {name}\n",
                    alias = alias, name = agg.canonical_name, in_ty = in_ty,
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

unsafe fn register_aggregate(conn: duckdb_connection, sql_name: &str, input_ty: &str) {
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

    // Input type from the shim's recorded param signature; most
    // postgis aggregates are unary blob (ST_Union, ST_Extent, ...)
    // but the mobilitydb F64 aggregates (tfloat_max_agg / etc.)
    // take a scalar f64. Return type stays BLOB by default — the
    // dispatch path converts every scalar result to bytes; users
    // who want the typed value can wrap with `tfloat_to_text` or
    // similar.
    let in_id = match input_ty {
        "float64" => ffi::DUCKDB_TYPE_DUCKDB_TYPE_DOUBLE,
        "float32" => ffi::DUCKDB_TYPE_DUCKDB_TYPE_FLOAT,
        "int64"   => ffi::DUCKDB_TYPE_DUCKDB_TYPE_BIGINT,
        "int32"   => ffi::DUCKDB_TYPE_DUCKDB_TYPE_INTEGER,
        "uint64"  => ffi::DUCKDB_TYPE_DUCKDB_TYPE_UBIGINT,
        "uint32"  => ffi::DUCKDB_TYPE_DUCKDB_TYPE_UINTEGER,
        "boolean" => ffi::DUCKDB_TYPE_DUCKDB_TYPE_BOOLEAN,
        "text"    => ffi::DUCKDB_TYPE_DUCKDB_TYPE_VARCHAR,
        _         => ffi::DUCKDB_TYPE_DUCKDB_TYPE_BLOB,
    };
    let in_lt = ffi::duckdb_create_logical_type(in_id);
    duckdb_aggregate_function_add_parameter(agg, in_lt);
    let ret_lt = ffi::duckdb_create_logical_type(ffi::DUCKDB_TYPE_DUCKDB_TYPE_BLOB);
    duckdb_aggregate_function_set_return_type(agg, ret_lt);
    // Both add_parameter and set_return_type take the type by
    // value and DuckDB clones it internally, so we still own
    // the handles and must destroy them after use.
    let mut in_to_destroy = in_lt;
    ffi::duckdb_destroy_logical_type(&mut in_to_destroy);
    let mut ret_to_destroy = ret_lt;
    ffi::duckdb_destroy_logical_type(&mut ret_to_destroy);

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
        // Scalar/aggregate name clashes (st_collect, st_union,
        // st_clusterwithin, st_clusterdbscan,
        // st_clusterintersecting, tfloat_sum, tint_sum, …) are
        // resolved in favour of the AGGREGATE: scalars.rs skips
        // any name also published as an aggregate, so this
        // registration normally succeeds. A non-success rc here
        // therefore means a genuine catalog conflict (e.g. a
        // built-in DuckDB aggregate of the same name) — log and
        // continue so the rest still register.
        eprintln!(
            "[shim-aggregates] could not register aggregate `{sql_name}` (rc={rc}); \
             a built-in of the same name may already exist"
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
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    states: *mut duckdb_aggregate_state,
) -> std::result::Result<(), String> {
    use datafission_functions::DataType;
    let n = ffi::duckdb_data_chunk_get_size(input) as usize;
    let v0 = ffi::duckdb_data_chunk_get_vector(input, 0);
    let validity = ffi::duckdb_vector_get_validity(v0);

    // Dispatch on the recorded input type. The aggregate's
    // ExtraInfo wraps `Arc<dyn AggregateFunctionDef>`; we read
    // its first parameter type to decide how to interpret the
    // vector's raw memory.
    let extra = extra_info_typed(info);
    let in_ty = extra.def.param_types()
        .first()
        .and_then(|sig| sig.first().cloned())
        .unwrap_or(DataType::Binary);

    macro_rules! prim_loop {
        ($ty:ty, $variant:ident) => {{
            let data = ffi::duckdb_vector_get_data(v0) as *const $ty;
            for i in 0..n {
                if !validity.is_null() && !ffi::duckdb_validity_row_is_valid(validity, i as idx_t) {
                    continue;
                }
                let val = std::ptr::read(data.add(i));
                let acc_ptr = read_state(states, i);
                let acc = &mut *acc_ptr;
                acc.accumulate(&FunctionValue::$variant(val))
                    .map_err(|e| format!("{e:?}"))?;
            }
        }};
    }

    match in_ty {
        DataType::Float64 => prim_loop!(f64, Float64),
        DataType::Float32 => prim_loop!(f32, Float32),
        DataType::Int64   => prim_loop!(i64, Int64),
        DataType::Int32   => prim_loop!(i32, Int32),
        DataType::UInt64  => prim_loop!(u64, UInt64),
        DataType::UInt32  => prim_loop!(u32, UInt32),
        DataType::Boolean => prim_loop!(bool, Boolean),
        DataType::Text => {
            let data = ffi::duckdb_vector_get_data(v0) as *const duckdb_string_t;
            for i in 0..n {
                if !validity.is_null() && !ffi::duckdb_validity_row_is_valid(validity, i as idx_t) {
                    continue;
                }
                let mut s_raw: duckdb_string_t = std::ptr::read(data.add(i));
                let bytes = read_string_t_bytes(&mut s_raw);
                let s = String::from_utf8_lossy(&bytes).into_owned();
                let acc_ptr = read_state(states, i);
                let acc = &mut *acc_ptr;
                acc.accumulate(&FunctionValue::String(s))
                    .map_err(|e| format!("{e:?}"))?;
            }
        }
        _ => {
            // Default: Binary/Blob — original path.
            let data = ffi::duckdb_vector_get_data(v0) as *const duckdb_string_t;
            for i in 0..n {
                if !validity.is_null() && !ffi::duckdb_validity_row_is_valid(validity, i as idx_t) {
                    continue;
                }
                let mut s_raw: duckdb_string_t = std::ptr::read(data.add(i));
                let bytes = read_string_t_bytes(&mut s_raw);
                let acc_ptr = read_state(states, i);
                let acc = &mut *acc_ptr;
                acc.accumulate(&FunctionValue::Binary(bytes))
                    .map_err(|e| format!("{e:?}"))?;
            }
        }
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
        // Output type is BLOB by construction (aggregates_rs sets the
        // return type to LogicalTypeId::Blob unconditionally). Encode
        // primitives as their little-endian bytes — callers that want
        // the typed value can apply `octet_length()` to confirm width
        // or unhex back to a primitive via DuckDB's bit ops.
        match value {
            FunctionValue::Binary(b)  => vector_assign_bytes(result, dst, &b),
            FunctionValue::String(s)  => vector_assign_bytes(result, dst, s.as_bytes()),
            FunctionValue::Float64(v) => vector_assign_bytes(result, dst, &v.to_le_bytes()),
            FunctionValue::Float32(v) => vector_assign_bytes(result, dst, &v.to_le_bytes()),
            FunctionValue::Int64(v)   => vector_assign_bytes(result, dst, &v.to_le_bytes()),
            FunctionValue::Int32(v)   => vector_assign_bytes(result, dst, &v.to_le_bytes()),
            FunctionValue::UInt64(v)  => vector_assign_bytes(result, dst, &v.to_le_bytes()),
            FunctionValue::UInt32(v)  => vector_assign_bytes(result, dst, &v.to_le_bytes()),
            FunctionValue::Boolean(v) => vector_assign_bytes(result, dst, &[v as u8]),
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

/// Map an interface-DB `TypeName` string to a generated
/// `datafission_functions::DataType` constructor expression. The
/// strings are the same vocabulary the scalar shape-classifier
/// uses (`binary`, `text`, `float64`, `int32`, …). Anything
/// unrecognised falls back to `DataType::Binary` — the shim ABI
/// is byte-oriented, so a BLOB-shaped argument round-trips an
/// arbitrary payload losslessly.
fn type_name_to_datatype_expr(t: &str) -> &'static str {
    match t {
        "boolean" => "DataType::Boolean",
        "int8"    => "DataType::Int8",
        "int16"   => "DataType::Int16",
        "int32"   => "DataType::Int32",
        "int64"   => "DataType::Int64",
        "uint8"   => "DataType::UInt8",
        "uint16"  => "DataType::UInt16",
        "uint32"  => "DataType::UInt32",
        "uint64"  => "DataType::UInt64",
        "float32" => "DataType::Float32",
        "float64" => "DataType::Float64",
        "text"    => "DataType::Text",
        "binary"  => "DataType::Binary",
        _         => "DataType::Binary",
    }
}

pub fn table_functions_rs(plan: &BridgePlan) -> String {
    let mut s = generated_header();
    s.push_str(
r##"//! Table-function (UDTF) registration via raw libduckdb-sys.
//!
//! ## Phase 5 (2026-06-25) — WIRED
//!
//! Each shim UDTF is registered through DuckDB's C
//! table-function API (`duckdb_create_table_function` + the
//! bind/init/function callbacks). DuckDB's `VTab` bind/init
//! hooks are *static* — they don't know which registered name
//! they're serving — so we dispatch dynamically by stashing the
//! resolved `Arc<dyn TableFunctionDef>` on the function's
//! ExtraInfo (mirrors how aggregates carry their def). One
//! generic dispatcher serves every UDTF regardless of arity or
//! signature, so a future shim's table functions wire up with
//! no per-shim code.
//!
//! ### One arity per name (DuckDB C-API limitation)
//!
//! Unlike the scalar C API (which has a function-set abstraction
//! for positional overloads), DuckDB's loadable table-function
//! C API binds by EXACT positional arity and keeps a single
//! catalog entry per name. We therefore register each name once,
//! with its NARROWEST recorded signature (the most broadly
//! callable form). Wider positional overloads — e.g.
//! `st_subdivide(geom, maxvertices)` on top of
//! `st_subdivide(geom)` — can't coexist under the same name and
//! are noted as comments in `register_all`. The chosen
//! signature's parameter types come from the interface DB.
//!
//! Lifecycle:
//!
//!   bind  — read each SQL parameter via `bind_get_parameter`
//!           (typed from `def.param_types()`), build the
//!           `FunctionValue` argument vector, derive the output
//!           column schema from `def.output_schema(input_types)`
//!           and declare it via `bind_add_result_column`. The
//!           args + the per-column output DataTypes are stored
//!           in the BindData.
//!
//!   init  — call `def.execute(&args)` to obtain a
//!           `Box<dyn TableFunctionIterator>`; park it (behind a
//!           Mutex, since DuckDB may call `function` from a
//!           worker thread) in the InitData. We force
//!           single-threaded scan (`init_set_max_threads(1)`)
//!           because the shim iterator is a stateful handle into
//!           the wasm Store and is not parallel-safe.
//!
//!   func  — pull `iter.next_row()` up to one vector's worth of
//!           rows, writing each column value to the matching
//!           output vector; set the produced chunk size. An
//!           empty chunk signals end-of-scan to DuckDB.

use std::ffi::{CString, c_char, c_void};
use std::sync::Arc;

use parking_lot::Mutex;

use libduckdb_sys::{
    self as ffi, DuckDBSuccess, duckdb_bind_add_result_column, duckdb_bind_get_extra_info,
    duckdb_bind_get_parameter, duckdb_bind_get_parameter_count, duckdb_bind_info,
    duckdb_bind_set_bind_data, duckdb_bind_set_error, duckdb_connection,
    duckdb_create_table_function, duckdb_data_chunk, duckdb_data_chunk_set_size,
    duckdb_destroy_table_function, duckdb_function_get_bind_data,
    duckdb_function_get_init_data, duckdb_function_info, duckdb_function_set_error,
    duckdb_init_get_bind_data, duckdb_init_info, duckdb_init_set_init_data,
    duckdb_init_set_max_threads, duckdb_register_table_function,
    duckdb_table_function_add_parameter, duckdb_table_function_set_bind,
    duckdb_table_function_set_extra_info, duckdb_table_function_set_function,
    duckdb_table_function_set_init, duckdb_table_function_set_name, idx_t,
};

use datafission_functions::DataType;
use datafission_functions::traits::{ColumnInfo, TableFunctionDef, TableFunctionIterator};
use datafission_functions::types::FunctionValue;

use crate::registry;

/// Per-registration state stashed on the table function via
/// `duckdb_table_function_set_extra_info`; recovered in `bind`.
///
/// `param_types` is the SPECIFIC overload signature this DuckDB
/// table-function registration serves. A UDTF with N distinct
/// argument-count overloads (e.g. `st_subdivide(blob)` and
/// `st_subdivide(blob, int32)`) is registered N times — once per
/// overload — each carrying its own `param_types` so `bind` reads
/// each positional argument with the right getter.
struct TfExtraInfo {
    def: Arc<dyn TableFunctionDef>,
    param_types: Vec<DataType>,
}

/// Produced by `bind`, consumed by `init` / `func`. Holds the
/// evaluated call arguments + the resolved def + the output
/// column DataTypes (so `func` knows how to write each column).
struct TfBindData {
    def: Arc<dyn TableFunctionDef>,
    args: Vec<FunctionValue>,
    out_types: Vec<DataType>,
}

/// Per-scan state produced by `init`. The shim iterator is a
/// stateful wasm handle, so it lives behind a Mutex and the scan
/// runs single-threaded.
///
/// `tag` is the leading field (and is always `INIT_OK_TAG`) so a
/// single-byte read at the box head can distinguish this from the
/// error sentinel `TfInitErr` — both arrive through the same
/// `duckdb_init_set_init_data` slot and `func` must tell them
/// apart. `#[repr(C)]` pins `tag` first.
#[repr(C)]
struct TfInitData {
    tag: u8,
    iter: Mutex<Box<dyn TableFunctionIterator>>,
    out_types: Vec<DataType>,
    done: bool,
}

/// Register every UDTF the shim publishes.
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
    let mut dropped_overloads = 0usize;
    for ext in &plan.extensions {
        for tf in &ext.table_functions {
            // DuckDB's loadable C table-function API binds by EXACT
            // positional arity and keeps a single catalog entry per
            // name — it has no positional-overload set like the
            // scalar API does. So we register ONE registration per
            // name, using the NARROWEST recorded signature (the most
            // broadly callable form). Wider positional overloads
            // (e.g. `st_subdivide(g, maxvertices)`) cannot coexist
            // under the same name through this API; they're listed
            // as a comment below and would need named-parameter
            // syntax to be reached. The chosen signature's parameter
            // TYPES come from the interface DB (the source of truth),
            // not the shim's runtime `param_types()` — the shim's WIT
            // `list-functions` metadata may advertise only one
            // canonical signature even when the SQL surface has more.
            let mut sigs: Vec<&Vec<String>> = tf.param_signatures.iter().collect();
            sigs.sort_by_key(|sig| sig.len());
            let chosen = sigs.first().copied();
            let dt_list = chosen
                .map(|sig| {
                    sig.iter()
                        .map(|t| type_name_to_datatype_expr(t))
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            s.push_str(&format!(
                "    register_table_function(conn, {name:?}, &[{dt_list}]);\n",
                name = tf.canonical_name, dt_list = dt_list,
            ));
            for alias in &tf.aliases {
                s.push_str(&format!(
                    "    register_table_function(conn, {alias:?}, &[{dt_list}]); // alias of {name}\n",
                    alias = alias, name = tf.canonical_name, dt_list = dt_list,
                ));
            }
            // Count + note any wider overloads we couldn't register.
            for sig in sigs.iter().skip(1) {
                s.push_str(&format!(
                    "    // overload `{name}({sig})` not registerable (DuckDB C API: \
                     one positional arity per table-function name)\n",
                    name = tf.canonical_name,
                    sig = sig.iter().cloned().collect::<Vec<_>>().join(", "),
                ));
                dropped_overloads += 1;
            }
            alias_count += tf.aliases.len();
            canonical += 1;
        }
    }
    s.push_str(&format!(
        "    // Phase 5: {canonical} canonical UDTFs + {alias_count} alias registrations \
         ({dropped_overloads} wider positional overloads not expressible via the C API).\n"
    ));
    if canonical == 0 {
        s.push_str("    // (no table functions in this interface DB)\n");
    }

    s.push_str(TABLE_FUNCTIONS_RUNTIME);

    s.push_str(
        "\n// ----------------------------------------------------------------------\n\
         // UDTFs the shim publishes.\n\
         // ----------------------------------------------------------------------\n\n",
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

/// The table-function runtime — the generic registrar + the
/// three C callbacks. Emitted verbatim into every generated
/// bridge so each crate is self-contained.
const TABLE_FUNCTIONS_RUNTIME: &str = r##"}

unsafe fn register_table_function(
    conn: duckdb_connection,
    sql_name: &str,
    param_types: &[DataType],
) {
    let def = match registry::lookup_table(sql_name) {
        Some(d) => d,
        None => {
            eprintln!("[shim-table-functions] no shim entry for `{sql_name}` — skipping");
            return;
        }
    };
    let param_types: Vec<DataType> = param_types.to_vec();

    let tf = duckdb_create_table_function();
    let name_cs = match CString::new(sql_name) {
        Ok(c) => c,
        Err(_) => {
            eprintln!("[shim-table-functions] name contains NUL: `{sql_name}` — skipping");
            duckdb_destroy_table_function(&mut { tf });
            return;
        }
    };
    duckdb_table_function_set_name(tf, name_cs.as_ptr());

    // Declare positional parameters for this overload. DuckDB
    // needs them to bind `SELECT * FROM fn(a, b, ...)`. The bind
    // callback re-reads the live values; these declarations fix
    // arity + coarse type so the planner accepts the call.
    for dt in &param_types {
        let lt = ffi::duckdb_create_logical_type(logical_type_id_for(dt));
        duckdb_table_function_add_parameter(tf, lt);
        let mut to_destroy = lt;
        ffi::duckdb_destroy_logical_type(&mut to_destroy);
    }

    duckdb_table_function_set_bind(tf, Some(bind_callback));
    duckdb_table_function_set_init(tf, Some(init_callback));
    duckdb_table_function_set_function(tf, Some(func_callback));

    // Park the per-registration def + this overload's param types.
    // The Box is leaked here and reclaimed when DuckDB calls the
    // destructor.
    let extra = Box::into_raw(Box::new(TfExtraInfo {
        def: Arc::clone(&def),
        param_types,
    }));
    duckdb_table_function_set_extra_info(tf, extra as *mut c_void, Some(drop_extra_info));

    let rc = duckdb_register_table_function(conn, tf);
    duckdb_destroy_table_function(&mut { tf });
    if rc != DuckDBSuccess {
        eprintln!(
            "[shim-table-functions] could not register `{sql_name}` (rc={rc})"
        );
    }
}

/// Map a DataFission `DataType` to the DuckDB C `DUCKDB_TYPE_*`
/// id used for both parameter declarations and output columns.
fn logical_type_id_for(dt: &DataType) -> ffi::duckdb_type {
    match dt {
        DataType::Boolean                  => ffi::DUCKDB_TYPE_DUCKDB_TYPE_BOOLEAN,
        DataType::Int8                     => ffi::DUCKDB_TYPE_DUCKDB_TYPE_TINYINT,
        DataType::Int16                    => ffi::DUCKDB_TYPE_DUCKDB_TYPE_SMALLINT,
        DataType::Int32                    => ffi::DUCKDB_TYPE_DUCKDB_TYPE_INTEGER,
        DataType::Int64                    => ffi::DUCKDB_TYPE_DUCKDB_TYPE_BIGINT,
        DataType::UInt8                    => ffi::DUCKDB_TYPE_DUCKDB_TYPE_UTINYINT,
        DataType::UInt16                   => ffi::DUCKDB_TYPE_DUCKDB_TYPE_USMALLINT,
        DataType::UInt32                   => ffi::DUCKDB_TYPE_DUCKDB_TYPE_UINTEGER,
        DataType::UInt64                   => ffi::DUCKDB_TYPE_DUCKDB_TYPE_UBIGINT,
        DataType::Float32                  => ffi::DUCKDB_TYPE_DUCKDB_TYPE_FLOAT,
        DataType::Float64                  => ffi::DUCKDB_TYPE_DUCKDB_TYPE_DOUBLE,
        DataType::Char { .. }
        | DataType::Varchar { .. }
        | DataType::Text                   => ffi::DUCKDB_TYPE_DUCKDB_TYPE_VARCHAR,
        DataType::Binary                   => ffi::DUCKDB_TYPE_DUCKDB_TYPE_BLOB,
        // Anything we don't have an explicit mapping for falls
        // back to BLOB — the shim ABI is byte-oriented, so a BLOB
        // column round-trips arbitrary payloads losslessly.
        _                                  => ffi::DUCKDB_TYPE_DUCKDB_TYPE_BLOB,
    }
}

// =====================================================================
// C callbacks. All `unsafe extern "C"` because DuckDB calls them.
// =====================================================================

unsafe extern "C" fn bind_callback(info: duckdb_bind_info) {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| bind_inner(info))) {
        Ok(Ok(())) => {}
        Ok(Err(msg)) => bind_set_err(info, &msg),
        Err(_)       => bind_set_err(info, "panic in table-function bind"),
    }
}

unsafe fn bind_inner(info: duckdb_bind_info) -> std::result::Result<(), String> {
    let extra = &*(duckdb_bind_get_extra_info(info) as *const TfExtraInfo);
    let def = Arc::clone(&extra.def);

    // This registration's specific overload signature tells us how
    // to interpret each positional argument's value.
    let want = extra.param_types.clone();

    let n = duckdb_bind_get_parameter_count(info) as usize;
    let mut args: Vec<FunctionValue> = Vec::with_capacity(n);
    let mut input_types: Vec<DataType> = Vec::with_capacity(n);
    for i in 0..n {
        let dt = want.get(i).cloned().unwrap_or(DataType::Binary);
        let val = duckdb_bind_get_parameter(info, i as idx_t);
        if val.is_null() {
            return Err(format!("table function `{}`: parameter {i} is null", def.name()));
        }
        let fv = read_value(val, &dt);
        let mut v = val;
        ffi::duckdb_destroy_value(&mut v);
        args.push(fv);
        input_types.push(dt);
    }

    // Derive + declare the output schema. An empty schema means
    // the shim couldn't express the output columns (e.g. an
    // input type not representable in the WIT logical-type set) —
    // surface that as a bind error rather than a 0-column table.
    let schema: Vec<ColumnInfo> = def.output_schema(&input_types);
    if schema.is_empty() {
        return Err(format!(
            "table function `{}`: shim returned an empty output schema",
            def.name()
        ));
    }
    let mut out_types: Vec<DataType> = Vec::with_capacity(schema.len());
    for col in &schema {
        let c_name = CString::new(col.name.as_str())
            .map_err(|_| format!("output column name contains NUL: {}", col.name))?;
        let lt = ffi::duckdb_create_logical_type(logical_type_id_for(&col.data_type));
        duckdb_bind_add_result_column(info, c_name.as_ptr(), lt);
        let mut to_destroy = lt;
        ffi::duckdb_destroy_logical_type(&mut to_destroy);
        out_types.push(col.data_type.clone());
    }

    let bind = Box::into_raw(Box::new(TfBindData { def, args, out_types }));
    duckdb_bind_set_bind_data(info, bind as *mut c_void, Some(drop_bind_data));
    Ok(())
}

unsafe extern "C" fn init_callback(info: duckdb_init_info) {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| init_inner(info))) {
        Ok(Ok(())) => {}
        Ok(Err(msg)) => {
            // init has no set_error of its own; surface via a
            // poisoned init-data carrying the message, picked up by
            // the first func call.
            let init = Box::into_raw(Box::new(TfInitErr { tag: INIT_ERR_TAG, msg }));
            duckdb_init_set_init_data(info, init as *mut c_void, Some(drop_init_err));
        }
        Err(_) => {
            let init = Box::into_raw(Box::new(TfInitErr {
                tag: INIT_ERR_TAG,
                msg: "panic in table-function init".into(),
            }));
            duckdb_init_set_init_data(info, init as *mut c_void, Some(drop_init_err));
        }
    }
}

unsafe fn init_inner(info: duckdb_init_info) -> std::result::Result<(), String> {
    let bind = &*(duckdb_init_get_bind_data(info) as *const TfBindData);
    let iter = bind.def.execute(&bind.args).map_err(|e| format!("{e:?}"))?;
    // The shim iterator is a single stateful wasm handle; force a
    // single-threaded scan so DuckDB doesn't call `func`
    // concurrently across worker threads.
    duckdb_init_set_max_threads(info, 1);
    let init = Box::into_raw(Box::new(TfInitData {
        tag: INIT_OK_TAG,
        iter: Mutex::new(iter),
        out_types: bind.out_types.clone(),
        done: false,
    }));
    duckdb_init_set_init_data(info, init as *mut c_void, Some(drop_init_data));
    Ok(())
}

unsafe extern "C" fn func_callback(info: duckdb_function_info, output: duckdb_data_chunk) {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| func_inner(info, output))) {
        Ok(Ok(())) => {}
        Ok(Err(msg)) => func_set_err(info, &msg),
        Err(_)       => func_set_err(info, "panic in table-function scan"),
    }
}

unsafe fn func_inner(
    info: duckdb_function_info,
    output: duckdb_data_chunk,
) -> std::result::Result<(), String> {
    // Both the success (TfInitData) and error (TfInitErr) paths
    // install a `#[repr(C)]` box whose first field is a `u8` tag.
    // Read that one byte to discriminate before interpreting the
    // rest of the allocation.
    let raw = duckdb_function_get_init_data(info);
    if raw.is_null() {
        // No init data at all — emit empty chunk = end of scan.
        duckdb_data_chunk_set_size(output, 0);
        return Ok(());
    }
    let tag = *(raw as *const u8);
    if tag == INIT_ERR_TAG {
        let err = &*(raw as *const TfInitErr);
        return Err(err.msg.clone());
    }

    let init = &*(raw as *const TfInitData);
    if init.done {
        duckdb_data_chunk_set_size(output, 0);
        return Ok(());
    }

    let cap = ffi::duckdb_vector_size() as usize;
    let ncols = init.out_types.len();
    let mut produced = 0usize;
    let mut exhausted = false;

    {
        let mut iter = init.iter.lock();
        while produced < cap {
            match iter.next_row() {
                Some(Ok(row)) => {
                    for c in 0..ncols {
                        let vec = ffi::duckdb_data_chunk_get_vector(output, c as idx_t);
                        let val = row.values.get(c).unwrap_or(&FunctionValue::Null);
                        write_cell(vec, produced, val, &init.out_types[c]);
                    }
                    produced += 1;
                }
                Some(Err(e)) => return Err(format!("{e:?}")),
                None => {
                    exhausted = true;
                    break;
                }
            }
        }
    }

    if exhausted {
        // Mark done so the next func call short-circuits. We need
        // interior mutability without a &mut; the bool sits behind
        // a raw write because DuckDB calls func single-threaded
        // for this scan (max_threads = 1).
        let init_mut = raw as *mut TfInitData;
        (*init_mut).done = true;
    }

    duckdb_data_chunk_set_size(output, produced as idx_t);
    Ok(())
}

/// Write `val` into the `row`-th slot of output vector `vec`,
/// using `out_ty` to pick the physical representation.
unsafe fn write_cell(vec: ffi::duckdb_vector, row: usize, val: &FunctionValue, out_ty: &DataType) {
    if matches!(val, FunctionValue::Null) {
        set_vector_null(vec, row);
        return;
    }
    match out_ty {
        DataType::Boolean => write_prim::<bool>(vec, row, val.as_bool().unwrap_or(false)),
        DataType::Int8    => write_prim::<i8>(vec, row, val.as_i64().unwrap_or(0) as i8),
        DataType::Int16   => write_prim::<i16>(vec, row, val.as_i64().unwrap_or(0) as i16),
        DataType::Int32   => write_prim::<i32>(vec, row, val.as_i64().unwrap_or(0) as i32),
        DataType::Int64   => write_prim::<i64>(vec, row, val.as_i64().unwrap_or(0)),
        DataType::UInt8   => write_prim::<u8>(vec, row, val.as_i64().unwrap_or(0) as u8),
        DataType::UInt16  => write_prim::<u16>(vec, row, val.as_i64().unwrap_or(0) as u16),
        DataType::UInt32  => write_prim::<u32>(vec, row, val.as_i64().unwrap_or(0) as u32),
        DataType::UInt64  => write_prim::<u64>(vec, row, val.as_i64().unwrap_or(0) as u64),
        DataType::Float32 => write_prim::<f32>(vec, row, val.as_f64().unwrap_or(0.0) as f32),
        DataType::Float64 => write_prim::<f64>(vec, row, val.as_f64().unwrap_or(0.0)),
        DataType::Char { .. } | DataType::Varchar { .. } | DataType::Text => {
            match val {
                FunctionValue::String(s) => assign_bytes(vec, row, s.as_bytes()),
                FunctionValue::Binary(b) => assign_bytes(vec, row, b),
                other => assign_bytes(vec, row, format!("{other:?}").as_bytes()),
            }
        }
        // BLOB / fallback — write the raw bytes.
        _ => match val {
            FunctionValue::Binary(b) => assign_bytes(vec, row, b),
            FunctionValue::String(s) => assign_bytes(vec, row, s.as_bytes()),
            FunctionValue::Int64(v)  => assign_bytes(vec, row, &v.to_le_bytes()),
            FunctionValue::Float64(v)=> assign_bytes(vec, row, &v.to_le_bytes()),
            FunctionValue::Boolean(v)=> assign_bytes(vec, row, &[*v as u8]),
            _ => set_vector_null(vec, row),
        },
    }
}

unsafe fn write_prim<T: Copy>(vec: ffi::duckdb_vector, row: usize, v: T) {
    let data = ffi::duckdb_vector_get_data(vec) as *mut T;
    std::ptr::write(data.add(row), v);
}

unsafe fn assign_bytes(vec: ffi::duckdb_vector, row: usize, bytes: &[u8]) {
    ffi::duckdb_vector_assign_string_element_len(
        vec,
        row as idx_t,
        bytes.as_ptr() as *const c_char,
        bytes.len() as idx_t,
    );
}

unsafe fn set_vector_null(vec: ffi::duckdb_vector, row: usize) {
    ffi::duckdb_vector_ensure_validity_writable(vec);
    let validity = ffi::duckdb_vector_get_validity(vec);
    if !validity.is_null() {
        ffi::duckdb_validity_set_row_invalid(validity, row as idx_t);
    }
}

/// Read a DuckDB `duckdb_value` parameter into a FunctionValue,
/// using the shim's declared parameter `DataType` to pick the
/// getter.
unsafe fn read_value(val: ffi::duckdb_value, dt: &DataType) -> FunctionValue {
    match dt {
        DataType::Boolean => FunctionValue::Boolean(ffi::duckdb_get_bool(val)),
        DataType::Int8    => FunctionValue::Int8(ffi::duckdb_get_int8(val)),
        DataType::Int16   => FunctionValue::Int16(ffi::duckdb_get_int16(val)),
        DataType::Int32   => FunctionValue::Int32(ffi::duckdb_get_int32(val)),
        DataType::Int64   => FunctionValue::Int64(ffi::duckdb_get_int64(val)),
        DataType::UInt8   => FunctionValue::UInt8(ffi::duckdb_get_uint8(val)),
        DataType::UInt16  => FunctionValue::UInt16(ffi::duckdb_get_uint16(val)),
        DataType::UInt32  => FunctionValue::UInt32(ffi::duckdb_get_uint32(val)),
        DataType::UInt64  => FunctionValue::UInt64(ffi::duckdb_get_uint64(val)),
        DataType::Float32 => FunctionValue::Float32(ffi::duckdb_get_float(val)),
        DataType::Float64 => FunctionValue::Float64(ffi::duckdb_get_double(val)),
        DataType::Char { .. } | DataType::Varchar { .. } | DataType::Text => {
            let p = ffi::duckdb_get_varchar(val);
            if p.is_null() {
                FunctionValue::String(String::new())
            } else {
                let s = std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned();
                ffi::duckdb_free(p as *mut c_void);
                FunctionValue::String(s)
            }
        }
        // BLOB / fallback — pull raw bytes.
        _ => {
            let blob = ffi::duckdb_get_blob(val);
            if blob.data.is_null() || blob.size == 0 {
                FunctionValue::Binary(Vec::new())
            } else {
                let bytes =
                    std::slice::from_raw_parts(blob.data as *const u8, blob.size as usize).to_vec();
                FunctionValue::Binary(bytes)
            }
        }
    }
}

// =====================================================================
// Init-data tagging.
//
// Both the success path (TfInitData) and the error path
// (TfInitErr) install a Box via `duckdb_init_set_init_data`. To
// tell them apart in `func` we put a `u8` tag as the first field
// of each so a single byte read at the box's head discriminates.
// =====================================================================

const INIT_OK_TAG: u8 = 0x0C;
const INIT_ERR_TAG: u8 = 0xE5;

#[repr(C)]
struct TfInitErr {
    tag: u8,
    msg: String,
}

unsafe extern "C" fn drop_extra_info(ptr: *mut c_void) {
    if ptr.is_null() { return; }
    drop(Box::from_raw(ptr as *mut TfExtraInfo));
}

unsafe extern "C" fn drop_bind_data(ptr: *mut c_void) {
    if ptr.is_null() { return; }
    drop(Box::from_raw(ptr as *mut TfBindData));
}

unsafe extern "C" fn drop_init_data(ptr: *mut c_void) {
    if ptr.is_null() { return; }
    drop(Box::from_raw(ptr as *mut TfInitData));
}

unsafe extern "C" fn drop_init_err(ptr: *mut c_void) {
    if ptr.is_null() { return; }
    drop(Box::from_raw(ptr as *mut TfInitErr));
}

unsafe fn bind_set_err(info: duckdb_bind_info, msg: &str) {
    if let Ok(cs) = CString::new(msg) {
        duckdb_bind_set_error(info, cs.as_ptr() as *const c_char);
    }
}

unsafe fn func_set_err(info: duckdb_function_info, msg: &str) {
    if let Ok(cs) = CString::new(msg) {
        duckdb_function_set_error(info, cs.as_ptr() as *const c_char);
    }
}
"##;

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
        // A non-success rc here means a type with this name already
        // exists in the catalog (e.g. DuckDB's built-in GEOMETRY
        // when the spatial extension is loaded, or a re-LOAD of
        // this bridge). Registration is IDEMPOTENT by design: the
        // type stays usable, and since both the pre-existing type
        // and our alias are BLOB-backed at the storage + ABI level,
        // `CREATE TABLE t (g {NAME})` and every BLOB-shaped scalar
        // signature keep working against the existing type. We
        // therefore reuse it rather than treat the clash as an
        // error. Downgraded to an informational note so it doesn't
        // read like a failure.
        eprintln!(
            "[shim-types] type {name} already registered (rc={rc}); reusing the \
             existing BLOB-compatible definition"
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

    // Collect, per extension, the PRIMARY spatial index — the first
    // declared row that carries capability metadata — plus its
    // capability flags. Drives which query table functions we emit.
    struct PrimaryIndex {
        prefix: String, // function-name prefix, e.g. "postgis"
        name: String,   // the builder alias passed to build_spatial_index
        knn: bool,
        within_distance_wkb: bool,
    }
    let mut primaries: Vec<PrimaryIndex> = Vec::new();
    for ext in &plan.extensions {
        let primary = ext
            .spatial_indexes
            .iter()
            .find(|ix| ix.capabilities_json.is_some());
        if let Some(ix) = primary {
            let caps = ix.capabilities_json.as_deref().unwrap_or("");
            // Lightweight flag extraction (avoids a serde_json dep in
            // the codegen). capabilities_json is a flat object of
            // boolean flags.
            let has = |k: &str| caps.contains(&format!("\"{k}\":true"));
            primaries.push(PrimaryIndex {
                prefix: sanitize_fn_prefix(&ext.name),
                name: ix.name.clone(),
                knn: has("knn"),
                within_distance_wkb: has("within_distance_wkb"),
            });
        }
    }

    s.push_str(
        r##"//! Spatial index build + query surface.
//!
//! ## What the loadable C API cannot do (documented limitation)
//!
//! DuckDB's stable loadable-extension C API (the only surface a
//! `--abi-type C_STRUCT` extension may call) has NO
//! `duckdb_register_index_type` (or equivalent). `CREATE INDEX …
//! USING <name>` resolves the access-method name against an
//! INTERNAL C++ registry a C-ABI extension cannot extend. So
//! `CREATE INDEX … USING rtree` / `… USING mobilitydb-strtree`
//! cannot be honoured by this bridge — that remains unsupported,
//! and DuckDB reports its own `Unknown index type`.
//!
//! ## What DOES work — the shim's tested build + query path
//!
//! The DataFission shim ships a complete, tested spatial-index
//! build + query path. The bridge's shim load
//! (`registry::load_shim` -> `RuntimeWasmExtension::register`)
//! installs the wasm-backed `SpatialIndexBuilder` into this
//! process's `datafission_index` registry under the aliases the
//! shim advertises. So `build_spatial_index(name, items)` routes
//! to the REAL shim R-tree / STRtree — no native reimplementation.
//!
//! We expose that through DuckDB as:
//!
//!   * a build AGGREGATE
//!       `<ext>_spatial_index_build(item_id BIGINT, geom_wkb BLOB)
//!         -> BIGINT`
//!     that accumulates (item_id, wkb) pairs, calls
//!     `build_spatial_index` at finalize, parks the resulting
//!     index in a session handle registry, and returns a u64
//!     HANDLE.
//!
//!   * query TABLE FUNCTIONS keyed by that handle:
//!       `<ext>_spatial_index_query_envelope(handle, min_x, min_y,
//!         max_x, max_y) -> (item_id BIGINT)`
//!       `<ext>_spatial_index_query_knn(handle, query_wkb, k)
//!         -> (item_id BIGINT)`            (if capabilities.knn)
//!       `<ext>_spatial_index_within_distance(handle, query_wkb,
//!         distance) -> (item_id BIGINT)`  (if within_distance_wkb)
//!
//! The build/query surface is `async`; the DuckDB callbacks are
//! sync, so calls are driven on a process-wide current-thread
//! tokio runtime via `block_on`.
//!
//! Everything here is interface-DB driven (the `spatial_indexes`
//! table + its `capabilities_json`): a future shim that declares a
//! spatial index gets this surface automatically.

use std::collections::HashMap;
use std::ffi::{c_char, c_void, CString};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use once_cell::sync::OnceCell;
use parking_lot::Mutex;

use libduckdb_sys::{
    self as ffi, duckdb_aggregate_function_add_parameter,
    duckdb_aggregate_function_get_extra_info, duckdb_aggregate_function_set_error,
    duckdb_aggregate_function_set_extra_info, duckdb_aggregate_function_set_functions,
    duckdb_aggregate_function_set_name, duckdb_aggregate_function_set_return_type,
    duckdb_aggregate_state, duckdb_bind_add_result_column, duckdb_bind_get_extra_info,
    duckdb_bind_get_parameter, duckdb_bind_get_parameter_count, duckdb_bind_info,
    duckdb_bind_set_bind_data, duckdb_bind_set_error, duckdb_connection,
    duckdb_create_aggregate_function, duckdb_create_table_function, duckdb_data_chunk,
    duckdb_data_chunk_get_size, duckdb_data_chunk_get_vector, duckdb_data_chunk_set_size,
    duckdb_destroy_aggregate_function, duckdb_destroy_table_function,
    duckdb_function_get_bind_data, duckdb_function_get_init_data, duckdb_function_info,
    duckdb_function_set_error, duckdb_init_get_bind_data, duckdb_init_info,
    duckdb_init_set_init_data, duckdb_init_set_max_threads, duckdb_register_aggregate_function,
    duckdb_register_table_function, duckdb_string_t, duckdb_table_function_add_parameter,
    duckdb_table_function_set_bind, duckdb_table_function_set_extra_info,
    duckdb_table_function_set_function, duckdb_table_function_set_init,
    duckdb_table_function_set_name, duckdb_vector, idx_t, DuckDBSuccess,
};

use datafission_index::spatial::build_spatial_index;
use datafission_index::{Envelope, SpatialIndex};

// =====================================================================
// Async runtime + session handle registry.
// =====================================================================

/// Process-wide current-thread tokio runtime used to drive the
/// async build/query calls from the sync DuckDB callbacks. The
/// wasm-backed builder/index do no real awaiting internally, so a
/// current-thread runtime is sufficient.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceCell<tokio::runtime::Runtime> = OnceCell::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("build current-thread tokio runtime")
    })
}

/// block_on a future on the shared runtime.
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    runtime().block_on(fut)
}

/// Session handle registry: u64 handle -> built index. The index
/// is the loader-backed `Arc<dyn SpatialIndex>` returned by
/// `build_spatial_index`, i.e. the real shim R-tree.
fn handles() -> &'static Mutex<HashMap<u64, Arc<dyn SpatialIndex>>> {
    static H: OnceCell<Mutex<HashMap<u64, Arc<dyn SpatialIndex>>>> = OnceCell::new();
    H.get_or_init(|| Mutex::new(HashMap::new()))
}

static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);

fn store_index(idx: Arc<dyn SpatialIndex>) -> u64 {
    let h = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    handles().lock().insert(h, idx);
    h
}

fn get_index(handle: u64) -> Option<Arc<dyn SpatialIndex>> {
    handles().lock().get(&handle).cloned()
}

/// Register the spatial-index build aggregate + query table
/// functions for every extension that declares a spatial index.
///
/// # Safety
///
/// `conn` must be a valid `duckdb_connection` for the duration of
/// this call.
pub unsafe fn register_all(conn: duckdb_connection) {
"##,
    );

    if primaries.is_empty() {
        s.push_str("    // (no spatial indexes in this interface DB)\n");
    }
    for p in &primaries {
        s.push_str(&format!(
            "    register_build_aggregate(conn, \"{prefix}_spatial_index_build\", {name:?});\n",
            prefix = p.prefix,
            name = p.name,
        ));
        s.push_str(&format!(
            "    register_query_tf(conn, \"{prefix}_spatial_index_query_envelope\", QueryKind::Envelope);\n",
            prefix = p.prefix,
        ));
        if p.knn {
            s.push_str(&format!(
                "    register_query_tf(conn, \"{prefix}_spatial_index_query_knn\", QueryKind::Knn);\n",
                prefix = p.prefix,
            ));
        }
        if p.within_distance_wkb {
            s.push_str(&format!(
                "    register_query_tf(conn, \"{prefix}_spatial_index_within_distance\", QueryKind::WithinDistance);\n",
                prefix = p.prefix,
            ));
        }
    }

    s.push_str(SPATIAL_INDEX_RUNTIME);

    s.push_str(
        "\n// ----------------------------------------------------------------------\n\
         // Spatial indexes the shim advertises.\n\
         // ----------------------------------------------------------------------\n\n",
    );
    for ext in &plan.extensions {
        s.push_str(&format!("// === extension: {} ===\n", ext.name));
        for ix in &ext.spatial_indexes {
            s.push_str(&format!(
                "// index `{}` type_id={} caps={}\n",
                ix.name,
                ix.type_id,
                ix.capabilities_json.as_deref().unwrap_or("(none)")
            ));
        }
    }
    s
}

/// The spatial-index runtime — the build aggregate + the query
/// table-function machinery + the C callbacks. Emitted verbatim
/// into every generated bridge so each crate is self-contained.
const SPATIAL_INDEX_RUNTIME: &str = r##"}

// =====================================================================
// Build AGGREGATE: (item_id BIGINT, geom_wkb BLOB) -> BIGINT handle.
//
// Accumulates (item_id, wkb) pairs; at finalize calls
// build_spatial_index(name, items) and returns a session handle.
// =====================================================================

/// Per-registration state stashed on the aggregate.
struct BuildExtraInfo {
    /// The builder alias passed to build_spatial_index (the shim's
    /// primary spatial-index name).
    index_name: String,
}

/// Per-group accumulator: the collected (item_id, wkb) pairs.
struct BuildState {
    items: Vec<(u64, Vec<u8>)>,
}

type BuildStatePtr = *mut BuildState;

unsafe fn register_build_aggregate(conn: duckdb_connection, sql_name: &str, index_name: &str) {
    let agg = duckdb_create_aggregate_function();
    let name_cs = match CString::new(sql_name) {
        Ok(c) => c,
        Err(_) => {
            eprintln!("[shim-spatial-index] name contains NUL: `{sql_name}`");
            duckdb_destroy_aggregate_function(&mut { agg });
            return;
        }
    };
    duckdb_aggregate_function_set_name(agg, name_cs.as_ptr());

    // (BIGINT item_id, BLOB geom_wkb) -> BIGINT handle.
    let id_lt = ffi::duckdb_create_logical_type(ffi::DUCKDB_TYPE_DUCKDB_TYPE_BIGINT);
    duckdb_aggregate_function_add_parameter(agg, id_lt);
    let wkb_lt = ffi::duckdb_create_logical_type(ffi::DUCKDB_TYPE_DUCKDB_TYPE_BLOB);
    duckdb_aggregate_function_add_parameter(agg, wkb_lt);
    let ret_lt = ffi::duckdb_create_logical_type(ffi::DUCKDB_TYPE_DUCKDB_TYPE_BIGINT);
    duckdb_aggregate_function_set_return_type(agg, ret_lt);
    let mut d = id_lt;
    ffi::duckdb_destroy_logical_type(&mut d);
    let mut d = wkb_lt;
    ffi::duckdb_destroy_logical_type(&mut d);
    let mut d = ret_lt;
    ffi::duckdb_destroy_logical_type(&mut d);

    duckdb_aggregate_function_set_functions(
        agg,
        Some(build_state_size),
        Some(build_state_init),
        Some(build_update),
        Some(build_combine),
        Some(build_finalize),
    );
    ffi::duckdb_aggregate_function_set_destructor(agg, Some(build_state_destroy));

    let extra = Box::into_raw(Box::new(BuildExtraInfo {
        index_name: index_name.to_string(),
    }));
    duckdb_aggregate_function_set_extra_info(agg, extra as *mut c_void, Some(drop_build_extra));

    let rc = duckdb_register_aggregate_function(conn, agg);
    duckdb_destroy_aggregate_function(&mut { agg });
    if rc != DuckDBSuccess {
        eprintln!("[shim-spatial-index] could not register build aggregate `{sql_name}` (rc={rc})");
    }
}

unsafe extern "C" fn build_state_size(_info: duckdb_function_info) -> idx_t {
    std::mem::size_of::<BuildStatePtr>() as idx_t
}

unsafe extern "C" fn build_state_init(_info: duckdb_function_info, state: duckdb_aggregate_state) {
    let boxed = Box::into_raw(Box::new(BuildState { items: Vec::new() }));
    std::ptr::write(state as *mut BuildStatePtr, boxed);
}

unsafe extern "C" fn build_state_destroy(states: *mut duckdb_aggregate_state, count: idx_t) {
    for i in 0..count as usize {
        let slot = ptr_read_state_slot(states, i);
        if !slot.is_null() {
            drop(Box::from_raw(slot));
        }
    }
}

/// `states` is an array of state pointers (slots). Read the i-th
/// slot, then read the BuildStatePtr stored in it.
unsafe fn ptr_read_state_slot(states: *mut duckdb_aggregate_state, idx: usize) -> BuildStatePtr {
    let slot = std::ptr::read(states.add(idx)) as *mut BuildStatePtr;
    std::ptr::read(slot)
}

unsafe extern "C" fn build_update(
    info: duckdb_function_info,
    input: duckdb_data_chunk,
    states: *mut duckdb_aggregate_state,
) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let n = duckdb_data_chunk_get_size(input) as usize;
        let id_vec = duckdb_data_chunk_get_vector(input, 0);
        let wkb_vec = duckdb_data_chunk_get_vector(input, 1);
        let ids = ffi::duckdb_vector_get_data(id_vec) as *const i64;
        for i in 0..n {
            if si_vector_row_is_null(id_vec, i) || si_vector_row_is_null(wkb_vec, i) {
                continue;
            }
            let item_id = std::ptr::read(ids.add(i)) as u64;
            let wkb = si_read_blob(wkb_vec, i);
            let acc = ptr_read_state_slot(states, i);
            if !acc.is_null() {
                (*acc).items.push((item_id, wkb));
            }
        }
    }))
    .map_err(|_| si_agg_err(info, "panic in spatial-index build update"));
}

unsafe extern "C" fn build_combine(
    _info: duckdb_function_info,
    source: *mut duckdb_aggregate_state,
    target: *mut duckdb_aggregate_state,
    count: idx_t,
) {
    for i in 0..count as usize {
        let src = ptr_read_state_slot(source, i);
        let tgt = ptr_read_state_slot(target, i);
        if !src.is_null() && !tgt.is_null() {
            let drained: Vec<(u64, Vec<u8>)> = std::mem::take(&mut (*src).items);
            (*tgt).items.extend(drained);
        }
    }
}

unsafe extern "C" fn build_finalize(
    info: duckdb_function_info,
    source: *mut duckdb_aggregate_state,
    result: duckdb_vector,
    count: idx_t,
    offset: idx_t,
) {
    let extra = &*(duckdb_aggregate_function_get_extra_info(info) as *const BuildExtraInfo);
    let out = ffi::duckdb_vector_get_data(result) as *mut i64;
    for i in 0..count as usize {
        let acc = ptr_read_state_slot(source, i);
        if acc.is_null() {
            si_set_vector_null(result, offset as usize + i);
            continue;
        }
        let items = std::mem::take(&mut (*acc).items);
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            block_on(build_spatial_index(&extra.index_name, items))
        }));
        match res {
            Ok(Ok(index)) => {
                let handle = store_index(index);
                std::ptr::write(out.add(offset as usize + i), handle as i64);
            }
            Ok(Err(e)) => {
                si_agg_err(info, &format!("build_spatial_index failed: {e:?}"));
                si_set_vector_null(result, offset as usize + i);
            }
            Err(_) => {
                si_agg_err(info, "panic in build_spatial_index");
                si_set_vector_null(result, offset as usize + i);
            }
        }
    }
}

// =====================================================================
// Query TABLE FUNCTIONS keyed by a handle.
//
// envelope: (handle BIGINT, min_x, min_y, max_x, max_y DOUBLE)
// knn:      (handle BIGINT, query_wkb BLOB, k INTEGER)
// within:   (handle BIGINT, query_wkb BLOB, distance DOUBLE)
// each yields one BIGINT `item_id` per hit.
// =====================================================================

#[derive(Clone, Copy)]
enum QueryKind {
    Envelope,
    Knn,
    WithinDistance,
}

struct QueryExtra {
    kind: QueryKind,
}

struct QueryBind {
    kind: QueryKind,
    handle: u64,
    // Envelope args (or the derived query-point envelope for within).
    env: Envelope,
    // knn / within payloads.
    wkb: Vec<u8>,
    k: usize,
    distance: f64,
}

#[repr(C)]
struct QueryInit {
    tag: u8,
    hits: Vec<u64>,
    pos: Mutex<usize>,
}

const SI_OK_TAG: u8 = 0x0C;
const SI_ERR_TAG: u8 = 0xE5;

#[repr(C)]
struct QueryInitErr {
    tag: u8,
    msg: String,
}

unsafe fn register_query_tf(conn: duckdb_connection, sql_name: &str, kind: QueryKind) {
    let tf = duckdb_create_table_function();
    let name_cs = match CString::new(sql_name) {
        Ok(c) => c,
        Err(_) => {
            eprintln!("[shim-spatial-index] name contains NUL: `{sql_name}`");
            duckdb_destroy_table_function(&mut { tf });
            return;
        }
    };
    duckdb_table_function_set_name(tf, name_cs.as_ptr());

    // Declare positional parameters by kind.
    let add = |id: ffi::duckdb_type| {
        let lt = ffi::duckdb_create_logical_type(id);
        duckdb_table_function_add_parameter(tf, lt);
        let mut d = lt;
        ffi::duckdb_destroy_logical_type(&mut d);
    };
    add(ffi::DUCKDB_TYPE_DUCKDB_TYPE_BIGINT); // handle
    match kind {
        QueryKind::Envelope => {
            add(ffi::DUCKDB_TYPE_DUCKDB_TYPE_DOUBLE); // min_x
            add(ffi::DUCKDB_TYPE_DUCKDB_TYPE_DOUBLE); // min_y
            add(ffi::DUCKDB_TYPE_DUCKDB_TYPE_DOUBLE); // max_x
            add(ffi::DUCKDB_TYPE_DUCKDB_TYPE_DOUBLE); // max_y
        }
        QueryKind::Knn => {
            add(ffi::DUCKDB_TYPE_DUCKDB_TYPE_BLOB); // query_wkb
            add(ffi::DUCKDB_TYPE_DUCKDB_TYPE_INTEGER); // k
        }
        QueryKind::WithinDistance => {
            add(ffi::DUCKDB_TYPE_DUCKDB_TYPE_BLOB); // query_wkb
            add(ffi::DUCKDB_TYPE_DUCKDB_TYPE_DOUBLE); // distance
        }
    }

    duckdb_table_function_set_bind(tf, Some(query_bind));
    duckdb_table_function_set_init(tf, Some(query_init));
    duckdb_table_function_set_function(tf, Some(query_func));

    let extra = Box::into_raw(Box::new(QueryExtra { kind }));
    duckdb_table_function_set_extra_info(tf, extra as *mut c_void, Some(drop_query_extra));

    let rc = duckdb_register_table_function(conn, tf);
    duckdb_destroy_table_function(&mut { tf });
    if rc != DuckDBSuccess {
        eprintln!("[shim-spatial-index] could not register query tf `{sql_name}` (rc={rc})");
    }
}

unsafe extern "C" fn query_bind(info: duckdb_bind_info) {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| query_bind_inner(info))) {
        Ok(Ok(())) => {}
        Ok(Err(msg)) => si_bind_err(info, &msg),
        Err(_) => si_bind_err(info, "panic in spatial-index query bind"),
    }
}

unsafe fn query_bind_inner(info: duckdb_bind_info) -> Result<(), String> {
    let extra = &*(duckdb_bind_get_extra_info(info) as *const QueryExtra);
    let kind = extra.kind;
    let nparams = duckdb_bind_get_parameter_count(info) as usize;

    let get_i64 = |i: usize| -> i64 {
        let v = duckdb_bind_get_parameter(info, i as idx_t);
        let r = ffi::duckdb_get_int64(v);
        let mut vv = v;
        ffi::duckdb_destroy_value(&mut vv);
        r
    };
    let get_i32 = |i: usize| -> i32 {
        let v = duckdb_bind_get_parameter(info, i as idx_t);
        let r = ffi::duckdb_get_int32(v);
        let mut vv = v;
        ffi::duckdb_destroy_value(&mut vv);
        r
    };
    let get_f64 = |i: usize| -> f64 {
        let v = duckdb_bind_get_parameter(info, i as idx_t);
        let r = ffi::duckdb_get_double(v);
        let mut vv = v;
        ffi::duckdb_destroy_value(&mut vv);
        r
    };
    let get_blob = |i: usize| -> Vec<u8> {
        let v = duckdb_bind_get_parameter(info, i as idx_t);
        let b = ffi::duckdb_get_blob(v);
        let bytes = if b.data.is_null() || b.size == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(b.data as *const u8, b.size as usize).to_vec()
        };
        let mut vv = v;
        ffi::duckdb_destroy_value(&mut vv);
        bytes
    };

    if nparams == 0 {
        return Err("spatial-index query: missing handle argument".into());
    }
    let handle = get_i64(0) as u64;

    let mut bind = QueryBind {
        kind,
        handle,
        env: Envelope::new(0.0, 0.0, 0.0, 0.0),
        wkb: Vec::new(),
        k: 0,
        distance: 0.0,
    };
    match kind {
        QueryKind::Envelope => {
            if nparams < 5 {
                return Err("query_envelope: expected (handle, min_x, min_y, max_x, max_y)".into());
            }
            bind.env = Envelope::new(get_f64(1), get_f64(2), get_f64(3), get_f64(4));
        }
        QueryKind::Knn => {
            if nparams < 3 {
                return Err("query_knn: expected (handle, query_wkb, k)".into());
            }
            bind.wkb = get_blob(1);
            bind.k = get_i32(2).max(0) as usize;
        }
        QueryKind::WithinDistance => {
            if nparams < 3 {
                return Err("within_distance: expected (handle, query_wkb, distance)".into());
            }
            bind.wkb = get_blob(1);
            bind.distance = get_f64(2);
            // Derive the query-point envelope from the WKB so we can
            // call query_within_distance_envelope.
            bind.env = wkb_point_envelope(&bind.wkb)
                .ok_or_else(|| "within_distance: query_wkb is not a parseable POINT".to_string())?;
        }
    }

    // Declare the single output column: item_id BIGINT.
    let col = CString::new("item_id").unwrap();
    let lt = ffi::duckdb_create_logical_type(ffi::DUCKDB_TYPE_DUCKDB_TYPE_BIGINT);
    duckdb_bind_add_result_column(info, col.as_ptr(), lt);
    let mut d = lt;
    ffi::duckdb_destroy_logical_type(&mut d);

    let boxed = Box::into_raw(Box::new(bind));
    duckdb_bind_set_bind_data(info, boxed as *mut c_void, Some(drop_query_bind));
    Ok(())
}

unsafe extern "C" fn query_init(info: duckdb_init_info) {
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| query_init_inner(info)));
    match res {
        Ok(Ok(())) => {}
        Ok(Err(msg)) => install_query_err(info, msg),
        Err(_) => install_query_err(info, "panic in spatial-index query init".into()),
    }
}

unsafe fn query_init_inner(info: duckdb_init_info) -> Result<(), String> {
    let bind = &*(duckdb_init_get_bind_data(info) as *const QueryBind);
    let index = get_index(bind.handle)
        .ok_or_else(|| format!("spatial-index handle {} not found", bind.handle))?;

    let hits: Vec<u64> = match bind.kind {
        QueryKind::Envelope => block_on(index.query_envelope(&bind.env))
            .map_err(|e| format!("query_envelope: {e:?}"))?,
        QueryKind::Knn => block_on(index.query_knn(&bind.wkb, bind.k))
            .map_err(|e| format!("query_knn: {e:?}"))?,
        QueryKind::WithinDistance => {
            block_on(index.query_within_distance_envelope(&bind.env, bind.distance))
                .map_err(|e| format!("within_distance: {e:?}"))?
        }
    };

    duckdb_init_set_max_threads(info, 1);
    let init = Box::into_raw(Box::new(QueryInit {
        tag: SI_OK_TAG,
        hits,
        pos: Mutex::new(0),
    }));
    duckdb_init_set_init_data(info, init as *mut c_void, Some(drop_query_init));
    Ok(())
}

unsafe fn install_query_err(info: duckdb_init_info, msg: String) {
    let err = Box::into_raw(Box::new(QueryInitErr {
        tag: SI_ERR_TAG,
        msg,
    }));
    duckdb_init_set_init_data(info, err as *mut c_void, Some(drop_query_init_err));
}

unsafe extern "C" fn query_func(info: duckdb_function_info, output: duckdb_data_chunk) {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| query_func_inner(info, output))) {
        Ok(Ok(())) => {}
        Ok(Err(msg)) => si_func_err(info, &msg),
        Err(_) => si_func_err(info, "panic in spatial-index query scan"),
    }
}

unsafe fn query_func_inner(
    info: duckdb_function_info,
    output: duckdb_data_chunk,
) -> Result<(), String> {
    let raw = duckdb_function_get_init_data(info);
    if raw.is_null() {
        duckdb_data_chunk_set_size(output, 0);
        return Ok(());
    }
    let tag = *(raw as *const u8);
    if tag == SI_ERR_TAG {
        let err = &*(raw as *const QueryInitErr);
        return Err(err.msg.clone());
    }
    let init = &*(raw as *const QueryInit);

    let cap = ffi::duckdb_vector_size() as usize;
    let vec = duckdb_data_chunk_get_vector(output, 0);
    let out = ffi::duckdb_vector_get_data(vec) as *mut i64;

    let mut pos = init.pos.lock();
    let mut produced = 0usize;
    while produced < cap && *pos < init.hits.len() {
        std::ptr::write(out.add(produced), init.hits[*pos] as i64);
        *pos += 1;
        produced += 1;
    }
    duckdb_data_chunk_set_size(output, produced as idx_t);
    Ok(())
}

// =====================================================================
// Minimal WKB POINT -> point-envelope parser (for within_distance).
//
// Supports a 2D POINT in WKB/EWKB: 1-byte byte-order, 4-byte type
// (low bits = 1 for POINT; EWKB SRID flag tolerated), then X,Y as
// 8-byte doubles. Returns a degenerate point envelope.
// =====================================================================

unsafe fn wkb_point_envelope(bytes: &[u8]) -> Option<Envelope> {
    // Not unsafe-dependent, but kept in the unsafe block region for
    // locality; pure byte parsing.
    if bytes.len() < 21 {
        return None;
    }
    let le = bytes[0] == 1;
    let rd_u32 = |o: usize| -> u32 {
        let b = [bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]];
        if le {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        }
    };
    let rd_f64 = |o: usize| -> f64 {
        let mut b = [0u8; 8];
        b.copy_from_slice(&bytes[o..o + 8]);
        if le {
            f64::from_le_bytes(b)
        } else {
            f64::from_be_bytes(b)
        }
    };
    let geom_type = rd_u32(1);
    // Low 16 bits hold the base type; POINT == 1. (EWKB packs SRID /
    // Z / M flags in the high bits, which we ignore for a 2D point.)
    if (geom_type & 0x000000ff) != 1 {
        return None;
    }
    // EWKB with SRID flag (0x20000000) inserts a 4-byte SRID before
    // the coordinates.
    let coord_off = if (geom_type & 0x20000000) != 0 { 9 } else { 5 };
    if bytes.len() < coord_off + 16 {
        return None;
    }
    let x = rd_f64(coord_off);
    let y = rd_f64(coord_off + 8);
    Some(Envelope::point(x, y))
}

// =====================================================================
// Shared helpers (prefixed `si_` to avoid clashing with other
// modules emitted into the same crate).
// =====================================================================

unsafe fn si_read_blob(vec: duckdb_vector, row: usize) -> Vec<u8> {
    let data = ffi::duckdb_vector_get_data(vec) as *const duckdb_string_t;
    let s = &*data.add(row);
    let len = s.value.inlined.length as usize;
    if len <= 12 {
        let inl = &s.value.inlined.inlined;
        std::slice::from_raw_parts(inl.as_ptr() as *const u8, len).to_vec()
    } else {
        std::slice::from_raw_parts(s.value.pointer.ptr as *const u8, len).to_vec()
    }
}

unsafe fn si_vector_row_is_null(vec: duckdb_vector, row: usize) -> bool {
    let validity = ffi::duckdb_vector_get_validity(vec);
    if validity.is_null() {
        return false;
    }
    !ffi::duckdb_validity_row_is_valid(validity, row as idx_t)
}

unsafe fn si_set_vector_null(vec: duckdb_vector, row: usize) {
    ffi::duckdb_vector_ensure_validity_writable(vec);
    let validity = ffi::duckdb_vector_get_validity(vec);
    if !validity.is_null() {
        ffi::duckdb_validity_set_row_invalid(validity, row as idx_t);
    }
}

unsafe fn si_agg_err(info: duckdb_function_info, msg: &str) {
    if let Ok(cs) = CString::new(msg) {
        duckdb_aggregate_function_set_error(info, cs.as_ptr() as *const c_char);
    }
}

unsafe fn si_bind_err(info: duckdb_bind_info, msg: &str) {
    if let Ok(cs) = CString::new(msg) {
        duckdb_bind_set_error(info, cs.as_ptr() as *const c_char);
    }
}

unsafe fn si_func_err(info: duckdb_function_info, msg: &str) {
    if let Ok(cs) = CString::new(msg) {
        duckdb_function_set_error(info, cs.as_ptr() as *const c_char);
    }
}

unsafe extern "C" fn drop_build_extra(ptr: *mut c_void) {
    if !ptr.is_null() {
        drop(Box::from_raw(ptr as *mut BuildExtraInfo));
    }
}

unsafe extern "C" fn drop_query_extra(ptr: *mut c_void) {
    if !ptr.is_null() {
        drop(Box::from_raw(ptr as *mut QueryExtra));
    }
}

unsafe extern "C" fn drop_query_bind(ptr: *mut c_void) {
    if !ptr.is_null() {
        drop(Box::from_raw(ptr as *mut QueryBind));
    }
}

unsafe extern "C" fn drop_query_init(ptr: *mut c_void) {
    if !ptr.is_null() {
        drop(Box::from_raw(ptr as *mut QueryInit));
    }
}

unsafe extern "C" fn drop_query_init_err(ptr: *mut c_void) {
    if !ptr.is_null() {
        drop(Box::from_raw(ptr as *mut QueryInitErr));
    }
}
"##;

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

/// Sanitize an extension name into a SQL-function-name prefix:
/// lowercase, with any non-alphanumeric run collapsed to `_`.
/// e.g. "mobilitydb" -> "mobilitydb", "My Ext" -> "my_ext".
fn sanitize_fn_prefix(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_us = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_us = false;
        } else if !prev_us {
            out.push('_');
            prev_us = true;
        }
    }
    out.trim_matches('_').to_string()
}

fn primary_extension_name(plan: &BridgePlan) -> String {
    plan.extensions.first().map(|e| e.name.clone()).unwrap_or_else(|| "shim".into())
}

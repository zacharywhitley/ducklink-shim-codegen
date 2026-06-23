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
crate-type = ["cdylib"]

[dependencies]
# TODO: pick a DuckDB-extension binding (duckdb-rs has loadable
# extension support; cargo-component if going via WASM).
# duckdb = {{ version = "1", features = ["bundled", "loadable-ext"] }}
# datafission-df-plugin-loader = {{ path = "../datafission/crates/df-plugin-loader" }}
# anyhow = "1"
"##,
        name = crate_name,
    )
}

pub fn lib_rs(plan: &BridgePlan) -> String {
    let mut s = generated_header();
    s.push_str(
r##"//! Generated DuckDB extension entry point.
//!
//! Load with:
//!   INSTALL '<crate>';
//!   LOAD '<crate>';

mod scalars;
mod aggregates;
mod table_functions;
mod window_functions;
mod types;
mod operators;
mod casts;
mod preprocessors;
mod system_catalog;
mod spatial_indexes;

// TODO: wire up the DuckDB C-extension entry points.
//
// The expected shape (loadable-extension ABI):
//
//   #[no_mangle]
//   pub extern "C" fn <name>_version_rust() -> *const c_char {
//       b"v1.0.0\0".as_ptr() as _
//   }
//
//   #[no_mangle]
//   pub extern "C" fn <name>_init_rust(db: ffi::duckdb_database) {
//       let con = unsafe { Connection::from_raw(db) };
//       // 1. Instantiate the wasm shim via df-plugin-loader.
//       // 2. Call types::register_all(&con, &ext)?   // register LogicalType::USER first
//       // 3. Call scalars::register_all(&con, &ext)?
//       // 4. Call aggregates::register_all(&con, &ext)?
//       // ...
//   }
//
"##,
    );
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
//! DuckDB API:
//!
//!   let mut sf = ScalarFunction::new("st_intersects");
//!   sf.add_parameter(LogicalType::User("GEOMETRY"));
//!   sf.add_parameter(LogicalType::User("GEOMETRY"));
//!   sf.set_return_type(LogicalType::Boolean);
//!   sf.set_function(Some(dispatch_fn));
//!   con.register_scalar_function::<MyDispatchState>(sf)?;
//!
//! The dispatch closure pulls args via DataChunk's vector
//! accessors, builds a batch, calls the shim, writes results
//! back into the output chunk. DuckDB ALREADY hands you N rows
//! per call — that's the natural batch boundary.

"##,
    );
    for ext in &plan.extensions {
        s.push_str(&format!("// === extension: {} ===\n", ext.name));
        for sc in &ext.scalars {
            let nargs = sc.param_signatures.first().map(|v| v.len()).unwrap_or(0);
            s.push_str(&format!(
                "// scalar `{}` (deterministic={}, propagates_null={}, arity={}, return={})\n",
                sc.canonical_name, sc.is_deterministic, sc.propagates_null, nargs, sc.return_type
            ));
            if !sc.aliases.is_empty() {
                s.push_str(&format!("//   aliases: {}\n", sc.aliases.join(", ")));
            }
        }
        s.push('\n');
    }
    s.push_str("// TODO: emit register_scalar_function calls.\n");
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

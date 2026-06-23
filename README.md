# ducklink-shim-codegen

Generate a DuckDB extension that bridges a DataFission wasm
shim (PostGIS, MobilityDB, …) into DuckDB as native functions,
aggregates, types, operators, and table functions.

## Usage

```
# 1. Produce the interface DB
extract-postgis-interface \
  --wasm /path/to/postgis-shim-composed.wasm \
  --output postgis.sqlite

# 2. Generate the bridge crate
ducklink-shim-codegen \
  --interface postgis.sqlite \
  --out ./postgis-duckdb-bridge

# 3. Build (TODO: today the output is a skeleton; see AGENTS.md)
cd postgis-duckdb-bridge && cargo build --release

# 4. Load into DuckDB
duckdb
> INSTALL './target/release/libpostgis_duckdb_bridge';
> LOAD './target/release/libpostgis_duckdb_bridge';
> SELECT ST_AsText(ST_Buffer(ST_GeomFromText('POINT(1 1)'), 0.5));
```

## What gets generated

The same crate skeleton shape as `sqlink-shim-codegen` outputs,
with DuckDB-flavoured TODO markers and call shapes:

```
postgis-duckdb-bridge/
├── Cargo.toml
├── README.md
└── src/
    ├── lib.rs               # duckdb_init + module wiring
    ├── scalars.rs           # ScalarFunction / register_scalar_function
    ├── aggregates.rs        # AggregateFunction
    ├── table_functions.rs   # TableFunction (UDTFs + catalog)
    ├── window_functions.rs
    ├── types.rs             # LogicalType::user("GEOMETRY")
    ├── operators.rs         # operator rewrite table for preprocessor
    ├── casts.rs             # register_cast_function (real DuckDB casts)
    ├── preprocessors.rs     # token rewrites
    ├── system_catalog.rs    # spatial_ref_sys via TableFunction
    └── spatial_indexes.rs   # IndexExtensionEntry per shim index
```

## What lives where

| Concern | Repo |
|---|---|
| Generic extractor + schema | `shim-interface-core` |
| Per-shim extractor binaries | `postgis-shim-interface` / `mobilitydb-shim-interface` |
| Read `.sqlite` → `BridgePlan` | `shim-bridge-codegen-core` |
| Emit SQLite extension code | `sqlink-shim-codegen` |
| **Emit DuckDB extension code** | **this repo** |

# Agent guide — ducklink-shim-codegen

This crate emits a DuckDB extension that bridges a DataFission
wasm shim into DuckDB. Today's emit is a **structural skeleton
with TODOs**; the implementation work below is what fills it in.

## Read this first

See `~/git/shim-bridge-codegen-core/PIPELINE.md` for the
six-repo map. This crate is one of two per-target codegens
(peer: `sqlink-shim-codegen`).

Pipeline:

```
shim.wasm
  └► postgis-shim-interface / mobilitydb-shim-interface  ─►  *.sqlite
        └► shim-bridge-codegen-core::load_plan          ─►  BridgePlan
              └► ducklink-shim-codegen (THIS REPO)       ─►  generated bridge crate
                    └► cargo build --release            ─►  libfoo_duckdb_bridge.so
                          └► DuckDB LOAD                 ─►  ST_Intersects etc. callable
```

## DuckDB-specific quirks worth knowing

These are the places DuckDB differs from SQLite, and where the
SQLite-side generator's notes do NOT apply:

1. **First-class custom types** via `LogicalType::user(name)`.
   Register `GEOMETRY`, `TGEOMPOINT`, etc. as proper types so
   `CREATE TABLE t (g GEOMETRY)` actually has type information
   the optimizer can use. Storage is still BLOB-like internally,
   but the surface is clean.

2. **CAST hooks** via `register_cast_function`. For cast rules
   with `source_kind == "any"`, this is the right place — much
   cleaner than the SQLite preprocessor path. For
   `stringliteral` / `geographycolumn` source kinds, you still
   need ducklink's preprocessor because DuckDB casts are
   type-driven, not expression-shape-driven.

3. **Vector-at-a-time execution**. The scalar dispatcher is
   called with N rows per call (chunk size, default 2048).
   That's the natural batch boundary — every dispatch maps
   1:1 to one shim call. SQLite, in contrast, dispatches one
   row at a time by default.

4. **Real operator support** via overloaded scalar functions
   bound to operator names. Still requires the preprocessor for
   shim-specific symbols (`<->`, `&&&`) that DuckDB's parser
   doesn't recognise. Standard symbols (`=`, `<`, `>`) on
   custom types DO work natively once the scalar function is
   registered.

5. **Window functions** are aggregates with framing hooks; the
   shim's H5 window-function plugins map naturally.

6. **Table functions** (`TableFunction`) are first-class — they
   have bind/init/function phases. UDTFs and system-catalog
   tables both go through this API.

## How a scalar is dispatched at runtime

Generated code for one scalar:

```rust
let mut sf = ScalarFunction::new("st_intersects");
sf.add_parameter(LogicalType::user("GEOMETRY"));
sf.add_parameter(LogicalType::user("GEOMETRY"));
sf.set_return_type(LogicalType::Boolean);
sf.set_function(Some(|info, input, output| {
    // input is a DataChunk with N rows
    let n = input.len();
    // Pull two BLOB vectors out of the chunk's columns:
    let lhs = input.flat_vector::<Vec<u8>>(0);
    let rhs = input.flat_vector::<Vec<u8>>(1);
    // Build a batch of N rows; call the shim once.
    // (See shim-bridge-codegen-core::marshal — today's
    // builder is a stub; until it's real, dispatch through
    // df-plugin-loader's scalar invoke helper per chunk.)
    let result = shim.scalar_invoke("st_intersects", &[lhs, rhs])?;
    output.write_vector(result);
}));
con.register_scalar_function::<()>(sf)?;
```

## TODO list — what to implement next

### Phase 1 — pick the DuckDB binding

DuckDB has multiple Rust extension paths:
- `duckdb` crate with the `loadable-ext` feature
- `cargo-component` + WASM (newer, less mature)

Pick one and pin it in `emit::cargo_toml`. The rest of the
phases follow from the choice.

### Phase 2 — scalar dispatch

- [ ] Real `LogicalType::user` registration in `types.rs`.
- [ ] Real `ScalarFunction::new` calls in `scalars.rs`, one per
      canonical + alias.
- [ ] Vector-at-a-time dispatcher closure that maps a chunk of N
      rows to one shim batch call.

### Phase 3 — aggregates / windows

- [ ] `AggregateFunction` with state_size / update / combine /
      finalize.
- [ ] Window-capable variants for those marked
      `supports_grouped == true`.

### Phase 4 — casts (this is where DuckDB beats SQLite)

- [ ] `register_cast_function` for every `source_kind == "any"`
      cast — real native CAST(x AS GEOMETRY) syntax just works.
- [ ] Preprocessor fallback for the other source kinds.

### Phase 5 — table functions

- [ ] UDTFs and system-catalog tables both via
      `register_table_function`.

### Phase 6 — spatial indexes

- [ ] Register `IndexExtensionEntry` per shim spatial index
      where the DuckDB version supports it. Fall back to UDTF
      + predicate pushdown via the bind callback on older
      versions.

## Things NOT to do

- **Don't fall back to BLOB without typing.** Use
  `LogicalType::user()` so SQL stays self-documenting.
- **Don't dispatch row-at-a-time.** DuckDB hands you N rows;
  use them.
- **Don't ignore the optimizer.** DuckDB's planner can push
  predicates into table functions when you tell it which args
  are filters; expose that via the bind callback for hot UDTFs.
- **Don't add a wasmtime dep here.** Codegen is pure-data;
  wasmtime belongs in the generated bridge crate.

## Verifying the skeleton compiles

```
cargo check         # this crate
cargo run -- --help # CLI works
cargo run -- --interface /tmp/postgis-interface.sqlite \
             --out /tmp/postgis-bridge-skel
```

## Reference points

- DuckDB extension docs: https://duckdb.org/docs/extensions/overview
- `duckdb-rs` loadable-extension example:
  https://github.com/duckdb/duckdb-rs/tree/main/examples
- The DataFission loader (`df-plugin-loader`): scalar/aggregate
  invoke helpers the dispatcher closures call.

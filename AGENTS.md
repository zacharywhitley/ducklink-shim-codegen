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

### Phase 1 — pick the DuckDB binding + scalar dispatch ✅ LANDED 2026-06-24

Binding choice: **`duckdb` crate v1.x with the `vscalar` +
`loadable-extension` features**, plus `duckdb-loadable-macros`
for the `duckdb_entrypoint_c_api` macro that emits the C-ABI
init symbol.

- [x] `Cargo.toml`: real path-deps into `datafission-df-plugin-loader`,
      `datafission-df-plugin-api`, `datafission-functions`;
      `duckdb` + `duckdb-loadable-macros` + `libduckdb-sys`.
- [x] `lib.rs`: `duckdb_entrypoint_c_api` macro expansion gives
      the C-ABI init symbol. The handler calls
      `registry::load_shim` then `scalars::register_all(&conn)`.
- [x] `registry.rs` (new emit): same once_cell-backed shim
      handle as sqlink. Reads composed wasm path from
      `<EXT>_SHIM_WASM`, drives `ext.register(&mut CapturingTarget)`
      to collect `ScalarFunctionDef`s, indexes by canonical +
      every alias.
- [x] `scalars.rs`: `TextToBlobScalar` marker struct impls
      `VScalar` with `State = Arc<dyn ScalarFunctionDef>`.
      `invoke` reads `duckdb_string_t` from the input
      `FlatVector`, calls `ScalarFunctionDef::execute` per row,
      writes the resulting `Vec<u8>` via `Inserter<&[u8]>`.
      Registration uses `register_scalar_function_with_state`
      so the same VScalar type can be re-registered under
      every canonical + alias name with a different Arc.
- [x] Verified end-to-end with the live PostGIS shim:
      ST_GeomFromText + 14 aliases all return correct WKB.
      Vectorised 100-row batch returns `n=100, total_bytes=2100`
      (matches SQLite Phase 1 output byte-for-byte).
      Invalid WKT propagates a clean error through anyhow →
      DuckDB binder.

#### Phase 1 runtime contract

Generated extensions need to be PACKAGED into a
`.duckdb_extension` file with the DuckDB metadata footer.
`cargo build` alone produces just a `.dylib` that DuckDB
rejects with `"The file is not a DuckDB extension. The
metadata at the end of the file is invalid"`. Use
`cargo-duckdb-ext-tools`:

```sh
# 1. Build the shim composed wasm (see sqlink AGENTS.md).

# 2. Build + package the bridge
cd $HOME/git/postgis-duckdb-bridge
cargo build --release

cargo install cargo-duckdb-ext-tools     # one-time
cargo duckdb-ext package \
  --library-path target/release/libpostgis_duckdb_bridge.dylib \
  --extension-path /tmp/postgis_duckdb_bridge.duckdb_extension \
  --extension-version v0.1.0 \
  --duckdb-platform osx_arm64 \
  --duckdb-version v1.5.2

# 3. Set env var + load (-unsigned because we don't sign)
export POSTGIS_SHIM_WASM=/tmp/postgis-shim-composed.wasm
duckdb -unsigned :memory: <<SQL
LOAD '/tmp/postgis_duckdb_bridge.duckdb_extension';
SELECT octet_length(ST_GeomFromText('POINT(1 1)'));       -- 21
SELECT hex(ST_GeomFromText('POINT(1 1)'));                -- 0101…F03F
SELECT typeof(ST_GeomFromText('POINT(1 1)'));             -- BLOB
SELECT octet_length(ST_GeomFromText('POLYGON((0 0, 4 0, 4 4, 0 4, 0 0))'));  -- 93
-- Vectorised over 100 rows in one batch:
SELECT count(*), sum(octet_length(ST_GeomFromText('POINT(' || i || ' ' || i || ')'))) FROM range(100) t(i);
SQL
```

Verified 2026-06-24 against DuckDB v1.5.2 on osx_arm64.

### Phase 2 — additional signature shapes ✅ LANDED 2026-06-24

Eight VScalar marker structs landed, covering the top eight
PostGIS scalar shapes (about 235 of 395 canonical scalars =
~59 % surface coverage):

  text         → blob    TextToBlobScalar      (ST_GeomFromText)
  blob         → blob    BlobToBlobScalar      (ST_Centroid)
  blob,blob    → bool    BlobBlobToBoolScalar  (ST_Intersects)
  blob         → f64     BlobToF64Scalar       (ST_Length, ST_Area)
  blob,f64     → blob    BlobF64ToBlobScalar   (ST_Buffer)
  blob         → text    BlobToTextScalar      (ST_AsText)
  blob,blob    → blob    BlobBlobToBlobScalar  (ST_Union)
  blob,blob    → f64     BlobBlobToF64Scalar   (ST_Distance)

Each shape has its own VScalar impl + `register_<shape>(conn,
name)` helper. The codegen's `classify_shape(sc)` matches
`(param_signatures[0].as_slice(), return_type.as_str())` and
emits one helper call per scalar.

A `lookup(sql_name)` helper centralises the registry lookup
+ error wrapping; `insert_blob_result::<S>` centralises the
blob-output write across the four shapes returning blob.

**Critical fix during Phase 2**: BLOB inputs MUST be read via
`DuckString::as_bytes()` not `as_str().as_bytes()` — the
`as_str` path does lossy UTF-8 conversion (replacing invalid
sequences with U+FFFD), which mangles WKB and produces
nonsense output. Took me 30 minutes of "POINT(-0.96875…)"
garbage to spot. Documented at every BLOB read site.

**Borrow-checker workaround for primitive output shapes**:
DuckDB's `FlatVector::as_mut_slice_with_len` and `set_null`
both borrow `&mut self` — can't have both alive in the same
scope. The shape impls collect null indices into a `Vec<usize>`
during the slice-write loop, then apply `set_null` in a second
pass after the slice borrow ends.

Verified end-to-end (each call goes through the full shim
dispatch):

  ST_AsText(ST_GeomFromText('POINT(1 1)'))           → POINT(1 1)
  ST_Length(LINESTRING(0 0,1 1,2 2))                 → 2.8284271…
  ST_Area(POLYGON((0 0,4 0,4 4,0 4,0 0)))            → 16
  ST_Distance(POINT(0 0), POINT(3 4))                → 5
  ST_Intersects(POLYGON(…), POINT(2 2))              → true
  ST_Intersects(POLYGON(…), POINT(10 10))            → false
  ST_Centroid(ST_Buffer(POINT(0 0), 1.0))            → POINT(0 0)
  -- Vectorised batch through ST_GeomFromText + ST_Area:
  SELECT sum(ST_Area(ST_GeomFromText('POLYGON((0 0,'||(i+1)||' 0,'||(i+1)||' '||(i+1)||',0 '||(i+1)||',0 0))')))
    FROM range(1,11) t(i);                            → 505 (= 2²+3²+…+11²)

### Phase 3 — remaining surface

Histogram of deferred shapes from `register_all`'s trailing
comment block. Top remaining shapes (each is a new VScalar
marker + register helper):

  blob          → uint32      (~13)  ST_NumPoints, ST_NumGeometries
  blob,uint32   → blob        (~12)  ST_GeometryN, ST_PointN
  blob,int32    → blob        (~11)  ST_SetSRID
  blob,f64,f64  → blob        (~11)  ST_Translate(2D)
  blob          → int32       (~8)
  blob,blob,f64 → bool        (~6)   ST_DWithin

Should add NULL-propagation handling per the new
`propagates_null` field on `ScalarFn` (today every shape
processes NULLs as zero-bytes; needs a check + early
set_null branch).

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

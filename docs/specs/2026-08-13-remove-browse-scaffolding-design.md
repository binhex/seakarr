# Seakarr — Remove Dead Browse Scaffolding

**Date:** 2026-08-13
**Status:** Approved

## 1. Background and decision

The iteration-3 design spec planned a peer-browse feature: `browse_user` on the
`SoulseekClient` trait, a `browse_cache` table, and `browse_cache_ttl_days` /
`browse_timeout_secs` config keys. During implementation the browse feature was
never wired in — nothing calls browse, and the pipeline (search → filter → rank →
download) works entirely from search-result metadata, with the fallback album-only
search handling banned artist+album criteria.

An investigation into adding browse caching concluded:

- **Search and browse are separate protocol actions.** Search is query-driven and
  returns only matching files. Browse requests a peer's *entire* share listing —
  a much larger payload, slower, and refusable by the peer.
- **Without a consumer, browse caching has no value.** The only scenarios where it
  would pay off are (a) batch/wantlist matching against cached listings, or
  (b) completeness rescue of partial search results. Both are bigger features
  with their own design questions.
- **YAGNI:** search (with the fallback album search) covers today's discovery
  needs. The dead scaffolding should be removed to keep the codebase clean.

**Decision:** drop the browse feature entirely and remove the dead scaffolding
(the `browse_cache` table and both browse config keys). No pipeline behaviour
changes — this is pure dead-code removal.

## 2. Scope

### In scope

1. `src/db.rs` — remove the `browse_cache` table DDL and its schema test
   assertion; add `DROP TABLE IF EXISTS browse_cache;` so existing databases
   (which already have the never-written table) are cleaned up on next startup.
2. `src/config.rs` — remove `browse_timeout_secs` and `default_browse_timeout()`
   from `DownloadConfig`; remove `browse_cache_ttl_days` and
   `default_browse_cache_ttl()` from `DatabaseConfig`; remove both from the
   test-default struct literal and from the embedded `seakarr.yml` template.
3. `src/download.rs` — remove `browse_timeout_secs: 60,` from the test
   `default_dl_config()`.

### Out of scope

- Historical documentation (`docs/specs/`, `docs/plans/`) is left untouched as a
  record of prior designs.
- No changes to search, fallback search, filters, ranking, downloads,
  organisation, notifications, or any other behaviour.
- No new browse feature or cache consumer — if wantlist matching or completeness
  rescue is ever wanted, it gets its own spec.

## 3. Exact changes

### 3.1 `src/db.rs`

Remove the table DDL:

```sql
CREATE TABLE IF NOT EXISTS browse_cache (
    username   TEXT NOT NULL,
    path       TEXT NOT NULL,
    data_json  TEXT NOT NULL,
    cached_at  TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (username, path)
);
```

Add to the schema init block:

```sql
DROP TABLE IF EXISTS browse_cache;
```

Remove the test line:

```rust
assert!(tables.contains(&"browse_cache".to_string()));
```

Safety of the drop: no code path ever inserted into `browse_cache`, so the table
is always empty in every existing database; dropping it cannot lose data. The
schema init uses `CREATE TABLE IF NOT EXISTS` with no `PRAGMA user_version`
migration mechanism, so a simple `DROP TABLE IF EXISTS` in init is the
project-consistent way to remove the table from existing installs.

### 3.2 `src/config.rs`

`DownloadConfig` — remove:

```rust
#[serde(default = "default_browse_timeout")]
pub browse_timeout_secs: u64,
```

`DatabaseConfig` — remove:

```rust
#[serde(default = "default_browse_cache_ttl")]
pub browse_cache_ttl_days: u32,
```

Remove the two default functions:

```rust
fn default_browse_timeout() -> u64 { 60 }
fn default_browse_cache_ttl() -> u32 { 7 }
```

Remove from the test-default struct literal:

```rust
browse_timeout_secs: default_browse_timeout(),
browse_cache_ttl_days: default_browse_cache_ttl(),
```

Remove from the embedded template:

```yaml
browse_timeout_secs: 60
browse_cache_ttl_days: 7
```

### 3.3 `src/download.rs`

Remove from the test-only `default_dl_config()`:

```rust
browse_timeout_secs: 60,
```

## 4. Backward compatibility

- User YAML files that still contain `browse_cache_ttl_days` or
  `browse_timeout_secs` keep loading: the config structs do not use
  `deny_unknown_fields`, so serde ignores unknown keys. On the next config
  rewrite the keys simply disappear.
- Existing SQLite databases lose the (empty) `browse_cache` table on first
  startup after this change.

## 5. Testing

1. **`cargo test`** — all existing tests pass with the removed fields/table
   (schema test updated, download test config updated).
2. **`cargo build --release`** — no dangling references to the removed fields.
3. **Schema check** — an in-memory test DB no longer contains `browse_cache`;
   the `DROP TABLE IF EXISTS` runs without error on a DB that already has the
   table (covered by the existing schema test if it creates and re-runs init,
   otherwise verified manually with a scratch DB).

## 6. Risks

- Minimal: dead-code removal only. The only behavioural effect is startup now
  drops an always-empty table.
- `browse_timeout_secs` is removed along with `browse_cache_ttl_days` because
  it exists solely for the never-implemented browse feature; no other code
  reads it (verified by grep before writing this spec).

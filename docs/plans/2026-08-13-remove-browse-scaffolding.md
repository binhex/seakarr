# Browse Scaffolding Removal Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use sub-agents (recommended) to implement this plan task-by-task.
> Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove all dead browse scaffolding — the never-written `browse_cache` SQLite table and the
unused `browse_timeout_secs` / `browse_cache_ttl_days` config keys — leaving search-driven downloads untouched.

**Architecture:** Pure dead-code removal across three files. The DB `migrate()` batch gains a
`DROP TABLE IF EXISTS browse_cache;` statement so existing databases (which already contain the
always-empty table) are cleaned on next startup; the schema test is extended to prove the drop path.

**Tech Stack:** Rust, rusqlite (SQLite), serde/serde_yaml (config). QC gates: `cargo fmt`,
`cargo clippy -p seakarr --all-targets -- -D warnings`, `cargo test`.

---

## Spec

Design spec (source of truth): `docs/specs/2026-08-13-remove-browse-scaffolding-design.md`

Decisions locked in the spec:

- Drop the browse feature entirely; remove the `browse_cache` table and both browse config keys.
- `DROP TABLE IF EXISTS browse_cache;` runs in `migrate()` so existing databases are cleaned.
  Safe: no code path ever inserted into the table.
- Old user YAMLs keep loading: config structs do not use `deny_unknown_fields`.
- No pipeline behaviour changes — pure dead-code removal.

---

### Task 1: db.rs — drop browse_cache in migrate(), remove its DDL

**Files:**

- Modify: `src/db.rs:144-150` (remove DDL block), `src/db.rs:81` (migrate batch — add DROP),
  `src/db.rs:313-335` (schema test)
- Test: `src/db.rs` (add one test, update `test_create_tables`)

- [ ] **Step 1: Write the failing test**

Add a test to the `#[cfg(test)] mod tests` block in `src/db.rs`, right after `fn test_db()`:

```rust
    #[test]
    fn test_browse_cache_dropped_from_existing_db() {
        // Simulate an existing install whose DB already has the browse_cache
        // table from before this feature was removed.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE browse_cache (
                username   TEXT NOT NULL,
                path       TEXT NOT NULL,
                data_json  TEXT NOT NULL,
                cached_at  TEXT NOT NULL DEFAULT (datetime('now')),
                PRIMARY KEY (username, path)
            );",
        )
        .unwrap();
        let db = Database { conn };
        db.migrate().unwrap();

        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='browse_cache'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "browse_cache table should be dropped by migrate()");
    }
```

Note: `Database { conn }` is constructed directly (fields are `pub`) instead of via
`open_in_memory()` because `open_in_memory()` calls `migrate()` immediately — we need the table
to exist *before* migrate runs.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p seakarr test_browse_cache_dropped_from_existing_db`
Expected: FAIL — `count` is 1 because migrate() currently neither drops the table nor is the DDL
removed yet.

- [ ] **Step 3: Implement the drop + remove the DDL**

In `src/db.rs` `migrate()`, add as the FIRST statement inside the `execute_batch(...)` string
(before `CREATE TABLE IF NOT EXISTS processed_albums`):

```sql
            DROP TABLE IF EXISTS browse_cache;
```

Delete the entire DDL block:

```rust
            CREATE TABLE IF NOT EXISTS browse_cache (
                username   TEXT NOT NULL,
                path       TEXT NOT NULL,
                data_json  TEXT NOT NULL,
                cached_at  TEXT NOT NULL DEFAULT (datetime('now')),
                PRIMARY KEY (username, path)
            );
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p seakarr test_browse_cache_dropped_from_existing_db`
Expected: PASS.

- [ ] **Step 5: Update the schema test**

In `test_create_tables`, change the comment from "Verify all 8 tables exist" to
"Verify all 7 tables exist" and delete this line:

```rust
        assert!(tables.contains(&"browse_cache".to_string()));
```

- [ ] **Step 6: Run the db test module**

Run: `cargo test -p seakarr -- db`
Expected: all db tests PASS.

- [ ] **Step 7: Commit**

```bash
git add src/db.rs
git commit -m "refactor: drop unused browse_cache table from schema"
```

---

### Task 2: config.rs — remove browse config keys and defaults

**Files:**

- Modify: `src/config.rs:123-124` (DownloadConfig field), `src/config.rs:141-142`
  (DatabaseConfig field), `src/config.rs:263-265` and `src/config.rs:284-286` (default fns),
  `src/config.rs:512` and `src/config.rs:521` (test struct literal), `src/config.rs:593` and
  `src/config.rs:602` (YAML template)

- [ ] **Step 1: Remove the two struct fields**

From `DownloadConfig`, delete:

```rust
    #[serde(default = "default_browse_timeout")]
    pub browse_timeout_secs: u64,
```

From `DatabaseConfig`, delete:

```rust
    #[serde(default = "default_browse_cache_ttl")]
    pub browse_cache_ttl_days: u32,
```

`DatabaseConfig` then contains only `path` — keep the struct and its serde derive as-is.

- [ ] **Step 2: Remove the two default functions**

Delete both functions:

```rust
fn default_browse_timeout() -> u64 {
    60
}
```

```rust
fn default_browse_cache_ttl() -> u32 {
    7
}
```

- [ ] **Step 3: Remove from the test struct literal**

In the `default_config()` test helper, delete:

```rust
                browse_timeout_secs: default_browse_timeout(),
```

and delete:

```rust
                browse_cache_ttl_days: default_browse_cache_ttl(),
```

so the `database:` literal becomes:

```rust
            database: DatabaseConfig {
                path: default_db_path(),
            },
```

- [ ] **Step 4: Remove from the embedded YAML template**

Delete from the `download:` section:

```yaml
  browse_timeout_secs: 60
```

Delete from the `database:` section:

```yaml
  browse_cache_ttl_days: 7
```

so the template's `database:` section is just:

```yaml
database:
  path: "db"
```

- [ ] **Step 5: Run tests to verify the expected compile failure**

Run: `cargo test -p seakarr config`
Expected: FAIL to compile — the only error is the dangling `browse_timeout_secs: 60,` field in
`src/download.rs` (test helper `default_dl_config`). If any *other* error appears, something was
missed — fix it in this task.

- [ ] **Step 6: Commit**

```bash
git add src/config.rs
git commit -m "refactor: remove unused browse config keys"
```

---

### Task 3: download.rs — remove dangling test field

**Files:**

- Modify: `src/download.rs:227` (test helper `default_dl_config`)

- [ ] **Step 1: Remove the field**

In the `#[cfg(test)]` helper `default_dl_config()`, delete:

```rust
            browse_timeout_secs: 60,
```

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test -p seakarr`
Expected: all seakarr tests PASS (compiles cleanly, no dangling references).

- [ ] **Step 3: Commit**

```bash
git add src/download.rs
git commit -m "test: drop browse_timeout_secs from download test config"
```

---

### Task 4: Full verification gates

- [ ] **Step 1: Format**

Run: `cargo fmt --check`
Expected: no output, exit 0. If not, run `cargo fmt`, then re-check, and include the formatted
files in the final commit.

- [ ] **Step 2: Lint**

Run: `cargo clippy -p seakarr --all-targets -- -D warnings`
Expected: exit 0, no warnings.

- [ ] **Step 3: Full test suite (workspace includes the vendored soulseek-rs-lib)**

Run: `cargo test`
Expected: all tests PASS, including the vendored crate's regression tests.

- [ ] **Step 4: Release build**

Run: `cargo build --release`
Expected: exit 0.

- [ ] **Step 5: Markdown lint (this plan file)**

Run: `markdownlint docs/plans/2026-08-13-remove-browse-scaffolding.md`
Expected: exit 0 (repo has `.markdownlintrc`: 120-char lines, tables exempt).

- [ ] **Step 6: Commit any gate-driven fixes**

```bash
git add -A
git commit -m "chore: final verification fixes for browse scaffolding removal"
```

(Only if Steps 1-5 produced changes; otherwise skip this commit.)

---

## Self-Review Notes

- **Spec coverage:** §2.1/§3.1 (DDL removal, DROP, test assertion, pre-existing-table test) →
  Task 1; §3.2 (both fields, both defaults, test literal, template) → Task 2; §3.3 (download
  test field) → Task 3; §4 backward compat (serde ignores unknown keys — no action, but the
  "compile error only at download.rs" check in Task 2 Step 5 proves no other consumer exists) →
  Task 2; §5 testing matrix → Task 4.
- **Type consistency:** field names quoted for removal match the current source exactly
  (`browse_timeout_secs`, `browse_cache_ttl_days`); no new symbols are introduced, so no
  cross-task signature drift is possible.
- **Ordering:** Tasks are ordered so the repo only fails to compile between Task 2 and Task 3
  (expected RED per TDD); each commit leaves the tree building.

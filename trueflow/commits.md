# Proposed Commits

## Commit 1: Fix unused variable warnings in TUI code

**Files:**
- `src/commands/tui.rs`

**Changes:**
- Prefix unused `code_height` parameters with underscore in `build_file_lines`, `build_directory_lines`, `build_root_lines`
- Remove `mut` from `entries_list` variable (line 1797)

**Message:**
```
Fix unused variable warnings in tui.rs

- Prefix unused code_height parameters with underscore
- Remove unnecessary mut from entries_list
```

---

## Commit 2: Fix disabled and loose test assertions

**Files:**
- `src/hashing.rs`
- `src/complexity.rs`

**Changes:**
- `hashing.rs`: Restore the stability test assertion that was commented out, with the correct hash value
- `complexity.rs`: Replace loose `assert!(score >= 3)` with exact `assert_eq!(score, 6)` and fix the comments explaining the calculation

**Message:**
```
Fix disabled and loose test assertions

- hashing: Restore stability test with correct expected hash
- complexity: Replace loose assertion with exact expected value

The stability test ensures fingerprint hashes don't change across versions,
which would break existing review records. The complexity test now verifies
the exact expected score (6) rather than a minimum (>=3).
```

---

## Commit 3: Add test helpers and improve error messages

**Files:**
- `tests/common/mod.rs`

**Changes:**
- Add `truncate()` helper function
- Add `is_gap()` helper for case-insensitive gap checking
- Add `block_kinds_without_gaps()` helper to extract non-gap block kinds
- Improve `json()` error message to include truncated output on parse failure
- Add comment explaining why `read_review_records()` skips invalid JSON

**Message:**
```
Add test helpers and improve JSON error messages

- Add is_gap() and block_kinds_without_gaps() helpers
- Include truncated output in JSON parse error messages
- Document why read_review_records skips invalid lines
```

---

## Commit 4: Clean up test duplication and dead code

**Files:**
- `tests/edge_cases.rs`
- `tests/tui_wiring.rs`

**Changes:**
- `edge_cases.rs`: Replace duplicate `TestEnv` struct with shared `TestRepo` from common module
- `tui_wiring.rs`: Remove no-op test `test_tui_mode_loads_without_error` that only called a function and asserted true

**Message:**
```
Remove test duplication and dead code

- edge_cases: Use shared TestRepo instead of duplicate TestEnv
- tui_wiring: Remove no-op test that provided no coverage
```

---

## Commit 5: Improve test robustness

**Files:**
- `tests/e2e_mark_store_coverage.rs`
- `tests/e2e_languages.rs`
- `tests/bug_regressions.rs`

**Changes:**
- `e2e_mark_store_coverage.rs`: Make GPG signing failure test accept various error messages (gpg, sign, key, spawn) to work across different environments
- `e2e_languages.rs`: Fail with context when expected files are missing from scan output; add count assertion
- `bug_regressions.rs`: Restore directory permissions before test cleanup to prevent orphaned unremovable directories; remove unused imports

**Message:**
```
Improve test robustness across environments

- GPG test: Accept various signing-related error messages
- Languages test: Fail explicitly when expected files are missing
- Permissions test: Restore permissions before cleanup
- Remove unused imports in bug_regressions.rs
```

---

## Commit 6: Use new test helpers

**Files:**
- `tests/e2e_markdown.rs`
- `tests/bug_regressions.rs`

**Changes:**
- `e2e_markdown.rs`: Replace manual gap filtering with `block_kinds_without_gaps()` helper
- `bug_regressions.rs`: Replace manual gap check with `is_gap()` helper

**Message:**
```
Use new test helpers for gap filtering

Replace manual eq_ignore_ascii_case("gap") patterns with shared helpers.
```

---

## Summary

| Commit | Files Changed | Description |
|--------|---------------|-------------|
| 1 | 1 | Fix compiler warnings |
| 2 | 2 | Fix disabled/loose assertions |
| 3 | 1 | Add test helpers |
| 4 | 2 | Remove duplication/dead code |
| 5 | 3 | Improve test robustness |
| 6 | 2 | Use new helpers |

**Total: 6 commits, 11 files**

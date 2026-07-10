# Issue #17: Targeted scans follow symlinks outside the repository

Status: ready
Date: 2026-07-10
Baseline commit: 9a98914698c4

## Problem

Worktree scanning has two different filesystem policies for the same repository entry. `trueflow review --all` reaches `scanner::scan_directory`; its `ignore::WalkBuilder` does not follow links and `collect_scan_inventory` accepts only entries whose walker-reported type is a regular file. A targeted review reaches `scanner::scan_paths`; that function joins an otherwise valid lexical `RepoPath` to the worktree and calls `std::fs::metadata`, which follows symbolic links. A repository path such as `src/link.rs` can therefore name an external file: `review --target main`, `review --target dirty`, and `review --target file:src/link.rs` can parse, return, and cache the external file's bytes, while `review --all` omits the same path.

The worktree-scanner policy must be explicit and uniform for stable filesystem states: after resolving the scan/repository root itself, a final link or symlinked ancestor observed beneath that root is rejected before content is opened or read. A link or ordinary replacement observed by later checks causes candidate bytes to be discarded, and a rejected link path has no scan-cache entry. Targeted scans should emit a path-scoped diagnostic because the caller explicitly selected the path. Full traversal may continue silently omitting stable links, as it does today, but must diagnose a regular entry that changes during admission or reading.

The guaranteed contract is deliberately limited: **on a quiescent filesystem, repository worktree scans exclude final and ancestor symlinks, including valid and broken links; when ordinary check/open/read drift is observed, they discard it instead of publishing or caching its bytes.** Repeated pathname checks are not atomic containment. They do not guarantee safety against an adversarial concurrent process that swaps components between checks while reproducing the compared metadata; that stronger threat model requires handle-relative no-follow traversal and platform file identities outside this issue.

## Evidence

- `trueflow/src/scanner.rs::scan_paths` canonicalizes the supplied root, derives `repo_path_base`, and joins each selected `RepoPath` to that base. Its `fs::metadata(&full_path)` call follows a final symlink and every symlinked ancestor. If followed metadata reports a file, its modification time and size become the `ScanInput` stamp, and the path can be reused from or inserted into the cache.
- `RepoPath::new` in `trueflow/src/repo_path.rs` rejects absolute paths plus `.` and `..` segments, but this is a lexical guarantee only. It cannot prevent `repo_path_base.join("linked-dir/file.rs")` from leaving the repository through `linked-dir`.
- `trueflow/src/scanner.rs::build_walker` constructs an `ignore::WalkBuilder` without enabling link following. `collect_scan_inventory` also rejects walker entries unless `DirEntry::file_type()` is a regular file. That is why a stable link is absent from a full scan.
- Full traversal is not completely protected by that first type check. `collect_scan_inventory` subsequently calls following `fs::metadata(entry.path())`, stores only a size/mtime `FileStamp`, and defers reading until `scan_file`. A regular entry replaced by a link between the walker type check, metadata call, cache decision, and content read can still be followed.
- `scanner::process_file` calls `analysis::analyze_file(path)` and then reopens the path with `fs::read(path)`. For an extensionless path, `analyze_file` itself opens and reads up to 8 KiB before `process_file` opens the path again. The bytes classified, the bytes hashed, and the metadata stamped are therefore not necessarily from the same file.
- `FileStamp` contains only nanosecond modification time and size. `scan_directory` reuses a `CachedFileEntry` when that stamp matches its inventory. `scan_paths` does the same after following metadata. The cache key and `FileState.path` remain the in-repository link name even when their content came from outside.
- Targeted cache mutation already has the right ownership model for eviction: `scan_paths` loads entries into `HashMap<RepoPath, CachedFileEntry>`, removes the selected key before replacement, and writes the remaining values through `finalize_scan_cache_write`. Its missing, ignored, and non-file branches already remove the selected key. The bug is that a valid file link reaches none of those branches.
- Full cache writing is replacement-based. `scan_directory` writes only entries from the current inventory, and `should_write_scan_cache` rewrites when the cached and current entry counts differ. Once links are excluded from admission, a full scan naturally prunes their old cache entries.
- `trueflow/src/vcs.rs::is_blob_change` deliberately treats `gix::object::tree::EntryKind::Link` as blob-like. `files_changed_main_to_head_in_repo`, revision-change selection, and range selection therefore can return a symlink path. `dirty_files` inserts every status item with a summary, and an explicit `file:` target is inserted directly by `targets::resolve_targets_with`. All selected worktree paths are consequently untrusted scanner inputs.
- `trueflow/src/targets.rs::{resolve_targets, ResolvedTargets::path_selection}` carries main/dirty changed paths and direct files into a scoped selection. `trueflow/src/commands/review.rs::preselected_paths_for_review` reduces those selections to a concrete `HashSet<RepoPath>`, and `collect_review` passes that set to `scanner::scan_paths`. `--all` instead passes through `scanner::scan_directory`.
- Historical revision content is a different boundary. `vcs::file_states_for_paths_in_revision` and `vcs::file_states_in_revision` read Git objects, and the latter explicitly handles `EntryKind::Link` as a Git blob. Those functions read the stored link text, not a worktree target, so they do not create this filesystem escape.
- Nearby test conventions are available without new infrastructure: scanner cache tests live in the private `scanner::tests` module and can inspect `load_cache_entry`; `trueflow/tests/vcs_scope.rs` creates real Git history with `TestRepo`; and `trueflow/tests/bug_regressions.rs` runs the real binary and parses review JSON. No symlink helper or symlink-specific regression currently exists.
- `trueflow/Cargo.toml` has no direct `libc`, `rustix`, `windows-sys`, capability-filesystem, or handle-relative-open dependency. The standard library provides `symlink_metadata`, `File::metadata`, and one-handle reads, which are sufficient for the requested stable-link policy and ordinary check/read drift defense without adding a heavy dependency.

## Reproduction

On Unix, run the following from the repository root. It creates an isolated repository, an external sentinel file, and a committed link selected by the main diff. It leaves the temporary paths in place for inspection.

```sh
cd trueflow
cargo build --bin trueflow
bin="$PWD/target/debug/trueflow"
repo="$(mktemp -d)"
outside="$(mktemp -d)"
home="$(mktemp -d)"
sentinel="TRUEFLOW_OUTSIDE_SYMLINK_SENTINEL_17"
git -C "$repo" init -q
git -C "$repo" config user.email test@example.com
git -C "$repo" config user.name "Test User"
mkdir -p "$repo/src"
printf 'pub fn base() {}\n' > "$repo/src/base.rs"
git -C "$repo" add src/base.rs
git -C "$repo" commit -q -m base
git -C "$repo" branch -M main
git -C "$repo" switch -q -c feature
printf 'pub fn escaped() { println!("%s"); }\n' "$sentinel" > "$outside/secret.rs"
ln -s "$outside/secret.rs" "$repo/src/link.rs"
git -C "$repo" add src/link.rs
git -C "$repo" commit -q -m 'add external link'
(
  cd "$repo"
  HOME="$home" "$bin" review --target main --json > main.json
  printf '%s\n' '--- targeted main review ---'
  cat main.json
  printf '%s\n' '--- sentinel in scan cache ---'
  grep -R "$sentinel" "$home/.trueflow/cache"
  HOME="$home" "$bin" review --all --json > all.json
  printf '%s\n' '--- full review ---'
  cat all.json
)
printf 'repository: %s\noutside: %s\nhome: %s\n' "$repo" "$outside" "$home"
```

At the baseline, the main-target JSON contains `src/link.rs` and the sentinel, and the first targeted scan writes the external content under the link path in the scan cache. The subsequent `--all` JSON omits `src/link.rs` because full traversal does not follow the directory entry. After the fix, both commands omit the link, the targeted command emits a link-specific warning on stderr, and neither JSON nor the cache contains the sentinel.

## Root cause

The code treats lexical repository membership as filesystem containment. `RepoPath` proves that a path string is relative and normalized, but a later following metadata/open operation can resolve any component somewhere else. The targeted scanner performs exactly that following operation and regards the target file's metadata as authority for the repository entry.

The full scanner happens to have a no-follow first stage because of the walker, but that policy is implicit and is discarded at the next `fs::metadata` call. Both scan modes then separate admission from use: inventory records a pathname and weak size/mtime stamp, cache selection happens later, analysis can open the name once, and content reading opens it again. There is no revalidation tying the admitted regular file to the handle that supplied the bytes.

VCS selection is not the security boundary. `EntryKind::Link` is a meaningful changed repository entry and should remain selectable so the user can receive a diagnostic about it. Removing `Link` from `is_blob_change` would only hide some main/revision/range link changes; it would not protect dirty selection, direct `file:` targets, feedback's targeted worktree loader, or callers of `scanner::scan_paths`. The worktree scanner must reject link paths regardless of how they were selected.

## Implementation plan

1. **Add observable red Unix regressions before changing scanner code.** Keep all Unix APIs behind whole-function `#[cfg(unix)]` attributes and use `std::os::unix::fs::symlink` locally; do not add an unconditional Unix import.
   1. In `trueflow/tests/vcs_scope.rs`, add `main_diff_selects_symlink_path_without_reading_it`. Build a base `main` commit, branch, add and commit `src/link.rs` as a filesystem symlink, call `files_changed_main_to_head_in_repo`, and assert the exact changed set contains `RepoPath("src/link.rs")`. This is a control that must already pass and must remain green: it proves the end-to-end main regression reaches scanner rejection rather than being “fixed” by dropping `EntryKind::Link` in VCS.
   2. In `trueflow/tests/bug_regressions.rs`, add a small Unix-only fixture helper that creates its sentinel file in a distinct temporary directory outside `TestRepo::path` and creates repository links with absolute targets. Give the sentinel a unique reviewable Rust body so any dereference is observable in raw JSON and block content. Keep the outside temporary directory alive for the test's lifetime.
   3. Add `review_main_rejects_stable_external_symlink_sentinel`. Commit the link on a feature branch relative to `main`, run the real `review --target main --json` binary, and assert success, valid JSON, no `src/link.rs` file, and no sentinel anywhere in stdout. Inspect stderr and require a path-scoped symbolic-link diagnostic. This must fail at the baseline because `is_blob_change` selects the link and `scan_paths` reads it.
   4. Add `review_dirty_rejects_stable_external_symlink_sentinel`. Use an untracked link so the baseline `dirty_files` index/worktree traversal selects it without depending on staged-status behavior. Run `review --target dirty --json` and make the same path, sentinel, and diagnostic assertions.
   5. Add `review_file_target_rejects_stable_external_symlink_sentinel`. Run `review --target file:src/link.rs --json`; this bypasses VCS classification and proves the scanner boundary protects direct callers for the stable fixture.
   6. In one of those fixtures, also run `review --all --json` as the full-traversal control and assert the same link path and sentinel are absent. The test should explicitly compare targeted and full file visibility, not merely assert that the full scan happened to return some other file.
   7. Run the focused integration commands below against the baseline. The VCS control must pass, the existing full-scan control must omit the link, and all three targeted cases must fail by exposing the sentinel/link path. Those contrasting results are the required red proof.

2. **Add focused scanner/cache red tests in `trueflow/src/scanner.rs::tests`.** Use explicit cache directories and inspect private cache structures; do not infer eviction only from an empty `ScanResult`.
   1. `scan_paths_rejects_external_symlink_and_evicts_cache`: first scan a regular `src/link.rs` through `scan_paths` with `ReadWrite` cache and assert it is included and cached. Replace that regular file with a symlink to an external sentinel, scan the same `RepoPath`, and assert: no files; a path-scoped link diagnostic; cache read `Hit`; cache write `Wrote`; zero reused files; zero rescanned files because rejection occurs before content scanning; and `load_cache_entry` contains no `src/link.rs` entry. Read the serialized cache file as an additional assertion that it contains neither the sentinel nor a cached `FileState` for the link.
   2. `scan_directory_prunes_cached_path_replaced_by_symlink`: warm two regular files with `scan_directory`, replace one with an external link, and rescan. Assert the real file is reused, the link is absent, the cache is rewritten because its entry count fell, and the loaded replacement cache contains only the real file. This protects full-inventory cache replacement and parity with targeted eviction.
   3. `scan_paths_rejects_file_beneath_symlinked_directory`: make an external directory containing the sentinel and request `linked/secret.rs`, where only `linked` is a repository symlink. Assert exclusion, a diagnostic naming the requested `RepoPath`, no sentinel-derived `FileState`, and no cache key. This prevents an insufficient fix that changes only the final `metadata` call to `symlink_metadata`.
   4. `scan_file_discards_path_replaced_by_symlink_after_inventory`: manually construct a private `ScanInput` and stamp while the path is a regular in-root file, replace the path with the external link, and invoke the internal scan attempt. Assert a non-cacheable rejection with a drift/link diagnostic and no included outcome. At the baseline, `scan_file` follows the replacement and returns an included `FileState`, so this is a deterministic check/read-drift regression without sleeps or timestamp-resolution assumptions.
   5. Cover a broken final link in the targeted test matrix. `symlink_metadata` can still identify it as a link; it must produce the same explicit no-follow rejection and cache eviction, not be conflated with an ordinary selected path that disappeared after a Git diff.
   6. `scan_cache_rejects_immediately_previous_format_entry_with_followed_link_bytes`: at the red-test step, record the checkout's current `SCAN_CACHE_FORMAT_VERSION` as the pre-fix version (it is 2 at the stated baseline, but may already be higher if issues #18 or #19 landed first). Create an ordinary in-root regular file and manually serialize a `CacheEntry` with that pre-fix version, the correct root/options fingerprints, a matching current `FileStamp`, and a poisoned `FileState` containing a same-length sentinel that represents bytes cached through the pre-fix following path. Scan the regular path and assert cache read `Miss`, one rescan, cache write `Wrote`, output derived from the actual safe file, and no sentinel. Before this issue's version increment, the entry is accepted and the matching stamp produces a `Hit`/reuse of the poisoned content; afterward it is exactly the immediately previous format and is rejected. This regression is platform-independent and proves the upgrade invalidates provenance-unknown caches even after a link has become a regular file.

3. **Centralize no-follow admission in `trueflow/src/scanner.rs`.** Introduce one private, explicit admission enum rather than Boolean combinations, for example `ScanPathAdmission::{Regular { full_path, stamp }, Link { component }, MissingOrNonFile}` with real I/O failures returned as `Result` errors.
   1. Starting from the already-established `repo_path_base`, walk every component of the normalized `RepoPath` and call `fs::symlink_metadata` on each accumulated prefix. Reject if any component's `file_type().is_symlink()` is true; require every non-final component to be a directory and the final component to be a regular file. Do not call `canonicalize` on the candidate, `read_link`, or following `metadata` as a substitute: those operations either follow the object or fail to detect an ancestor link.
   2. Keep root handling explicit. `canonicalize_or_original` and `repo_path_base_for_scan_root` establish the trusted scan base once; the new no-follow rule applies to components below that base. Do not change the meaning of a caller that supplied the repository root through a symlink as part of this issue.
   3. Factor `FileStamp` construction from a borrowed `Metadata` into one private helper so inventory, handle validation, and post-read validation compare the same size/mtime representation. Increment the checkout's current `SCAN_CACHE_FORMAT_VERSION` exactly once before loading fixed-policy caches. The stated baseline value is 2, so this issue alone would make it 3; if issue #18 or #19 has already incremented the constant, use that landed value as $N$ and set this issue's value to $N+1$ rather than forcing it back to 3. Every entry from the immediately previous format has unknown symlink provenance because it was writable before this fix; reject all of them even if the current path is now a regular file with the same size/mtime. Do not migrate or reinterpret previous-format entries: a miss and ordinary rescan is the clean beta cutover.
   4. In `scan_paths`, run admission before cache lookup/reuse. `Regular` continues to the existing stamp reuse/rescan logic. `Link` removes any loaded entry, adds a deterministic `ScanDiagnostic` for the selected `RepoPath` (reason includes “symbolic link”), and continues without incrementing reused/rescanned counts. `MissingOrNonFile` preserves current deleted/non-file skip behavior while evicting the key. Unexpected metadata/permission failures retain the existing diagnostic/logging path and must not preserve stale selected content.
   5. In `build_walker`, set link following to false explicitly instead of relying on the dependency default. In `collect_scan_inventory`, retain the walker type check as a cheap first filter, normalize the entry to `RepoPath`, and then run the shared component-wise admission helper instead of `fs::metadata(entry.path())`. A stable walker-visible link remains a silent omission; a walker entry that was reported as regular but no longer admits as the same regular candidate produces a diagnostic and is excluded.

4. **Bind content processing to one file handle and make observed drift non-cacheable.** Use only `std` I/O and an explicit internal result enum such as `ScanAttempt::{Cache(CachedFileEntry), Reject { diagnostics }}` so ordinary rejection paths do not persist a detected link/drift result.
   1. Immediately before opening an uncached `ScanInput`, repeat component-wise admission and require its `FileStamp` to match the inventory stamp. This rejects stable replacement links before content read and detects ordinary replacements that remain present at the check; it does not close the race between that pathname check and `File::open`.
   2. Open the path exactly once with `std::fs::File`. Compare `File::metadata()` with the admitted regular-file stamp, then repeat path admission after open and before `read_to_end`. If those observations show that the handle or pathname no longer matches, return `ScanAttempt::Reject` before reading content.
   3. Read the handle once into one `Vec<u8>`. After the read, compare handle metadata and component-wise path admission with the original stamp again. When either check observes a change, discard the buffer, return a path-scoped “changed during scan” diagnostic, and do not create a `CachedFileEntry`; rejected bytes do not proceed to language analysis, hashing, block splitting, `ScanResult.files`, or cache serialization.
   4. Preserve cacheable `CachedFileOutcome::Skipped` for stable regular files whose contents are unsupported, invalid UTF-8, or otherwise fail existing processing; existing diagnostic-cache behavior depends on it. Reserve non-cacheable `ScanAttempt::Reject` for observed admission/link/drift failures so no detected link key is inserted by either `scan_paths` or `scan_directory`.
   5. Make both callers handle the enum exhaustively: append and persist `Cache` outcomes as today; append only diagnostics for `Reject`, leave the targeted map key absent, and omit the full-inventory entry from the replacement vector.
   6. State the threat boundary in the helper/enum documentation: these pathname observations enforce stable-link exclusion and best-effort ordinary drift detection, not adversarial race containment. Do not describe `repo_path_base`, a matching `FileStamp`, or post-open revalidation as proof that an opened handle remained beneath the root.

5. **Remove the second pathname open in `trueflow/src/analysis.rs` and `scanner::process_file`.** This is part of the correctness fix, not unrelated cleanup.
   1. Change the sole production analysis API to accept both the path and the already-read bytes (for example, `analyze_file(path, bytes)`). Keep extension and canonical-filename precedence exactly as today; for unknown names, inspect at most the first 8 KiB of that slice for NUL and shebang detection.
   2. Update `scanner::process_file` to accept the normalized `RepoPath` plus the validated byte slice. It must classify, hash, UTF-8-decode, and split that same buffer without any filesystem access.
   3. Update the existing analysis tests for binary NUL detection, extensionless shebangs, canonical `Justfile`, and extension classification to pass bytes directly. Do not leave a path-opening wrapper or deprecated alias: scanner is the only current production caller, the project is beta, and a compatibility API would preserve the unsafe reopen path.

6. **Add platform-gated coverage without weakening production behavior.** Keep the required external-sentinel integration matrix Unix-only with `#[cfg(unix)]`. In scanner unit tests, add a `#[cfg(windows)]` equivalent using `std::os::windows::fs::symlink_file`; if setup returns `PermissionDenied` or `Unsupported`, return from that test with a clear comment because Windows symlink creation can require Developer Mode or privilege. Any other setup error must fail. The production admission helper itself remains cross-platform `std::fs::symlink_metadata` code and is never gated out on Windows. Do not make Windows follow links merely because a test host cannot create one.

7. **Run focused green smoke checks immediately after the source fix.** Run the main/dirty/direct CLI matrix, scanner admission/cache/drift tests, the poisoned-immediately-previous-format cache regression, and analysis byte-classification tests. Rerun the stable reproduction before broader validation and inspect stdout, stderr, and the cache: all worktree modes omit the fixture link content, targeted modes warn, and the external sentinel appears nowhere.

8. **Cleanup only after the focused smoke is green.** Remove obsolete `fs::metadata`/`fs::read` calls and imports from the scanner read path, keep admission and stamp construction private, and ensure diagnostic text is deterministic enough for path/reason assertions. Do not change VCS link selection, target resolution, historical Git-object scanning, cache fields beyond the required format-version bump, or root canonicalization. Then run the complete touched test binaries and final repository gate.

Ordered affected files and symbols:

1. `trueflow/tests/vcs_scope.rs` — Unix-gated `main_diff_selects_symlink_path_without_reading_it` control for `EntryKind::Link` selection.
2. `trueflow/tests/bug_regressions.rs` — Unix external-sentinel fixture plus main, dirty, direct-file, and full-review regressions using the real CLI.
3. `trueflow/src/scanner.rs` tests — final-link, ancestor-link, broken-link, targeted/full cache eviction, poisoned immediately-previous-format cache rejection, deterministic inventory/read drift, and Windows-gated coverage.
4. `trueflow/src/scanner.rs` implementation — `SCAN_CACHE_FORMAT_VERSION`, `build_walker`, `collect_scan_inventory`, `scan_paths`, `FileStamp` construction, `scan_file`, `process_file`, link/drift diagnostics, and explicit non-cacheable scan-attempt handling.
5. `trueflow/src/analysis.rs` — byte-slice analysis API and its existing binary/shebang/name/extension tests.

Inspected boundaries that should remain behaviorally unchanged:

- `trueflow/src/vcs.rs::{is_blob_change, collect_changed_paths, dirty_files, file_states_for_paths_in_revision, file_states_in_revision}`;
- `trueflow/src/targets.rs::{resolve_targets, resolve_targets_with, ResolvedTargets::path_selection}`;
- `trueflow/src/commands/review.rs::{collect_review, preselected_paths_for_review}`;
- `trueflow/src/feedback_export.rs::{load_snapshot_files_strict, load_snapshot_files_for_paths_strict}`; its worktree callers receive the scanner fix automatically.

## Verification and validation

Run these commands from the repository root, in order.

1. VCS `EntryKind::Link` selection control:

   ```sh
   cd trueflow && cargo test --features tui-test-support --test vcs_scope main_diff_selects_symlink_path_without_reading_it -- --exact --nocapture
   ```

2. Red/green real-CLI main, dirty, direct-file, and full-scan sentinel matrix:

   ```sh
   cd trueflow && cargo test --features tui-test-support --test bug_regressions external_symlink_sentinel -- --nocapture
   ```

3. Targeted admission, ancestor-link, broken-link, cache eviction, full-cache pruning, and deterministic drift checks:

   ```sh
   cd trueflow && cargo test --features tui-test-support --lib scanner::tests::scan_paths_rejects_external_symlink_and_evicts_cache -- --exact --nocapture
   cd trueflow && cargo test --features tui-test-support --lib scanner::tests::scan_directory_prunes_cached_path_replaced_by_symlink -- --exact --nocapture
   cd trueflow && cargo test --features tui-test-support --lib scanner::tests::scan_paths_rejects_file_beneath_symlinked_directory -- --exact --nocapture
   cd trueflow && cargo test --features tui-test-support --lib scanner::tests::scan_file_discards_path_replaced_by_symlink_after_inventory -- --exact --nocapture
   cd trueflow && cargo test --features tui-test-support --lib scanner::tests::scan_cache_rejects_immediately_previous_format_entry_with_followed_link_bytes -- --exact --nocapture
   ```

4. Byte-based analysis behavior and existing scanner-cache controls:

   ```sh
   cd trueflow && cargo test --features tui-test-support --lib analysis::tests::analyze_file_ -- --nocapture
   cd trueflow && cargo test --features tui-test-support --lib scanner::tests::scan_paths_writes_and_reuses_requested_file_cache -- --exact
   cd trueflow && cargo test --features tui-test-support --lib scanner::tests::scan_paths_preserves_unrequested_cache_entries -- --exact
   cd trueflow && cargo test --features tui-test-support --lib scanner::tests::scan_cache_rewrites_when_cached_file_is_deleted -- --exact
   ```

5. Focused behavioral smoke: rerun `## Reproduction`. Confirm both commands exit successfully; main-target and full JSON omit `src/link.rs`; the main-target stderr diagnostic names `src/link.rs` and symbolic-link rejection; and recursive sentinel search under the isolated cache has no match. Repeat in that fixture with an untracked link and `review --target dirty --json`, and with `review --target file:src/link.rs --json`.

6. All touched unit/integration targets, including platform-gated compilation:

   ```sh
   cd trueflow && cargo test --features tui-test-support --lib analysis::tests
   cd trueflow && cargo test --features tui-test-support --lib scanner::tests
   cd trueflow && cargo test --features tui-test-support --test vcs_scope --test bug_regressions
   ```

7. Final repository gate:

   ```sh
   just check
   ```

## Acceptance criteria

- On a quiescent filesystem, `scan_paths` rejects a final symbolic link and any path beneath a symlinked directory before content analysis or reading; broken links are recognized as links rather than ordinary missing files.
- `scan_directory` explicitly uses the same stable-state no-follow admission and revalidates walker entries without a following `fs::metadata` call. Targeted and full worktree scans agree that the fixture link entries have no `FileState`.
- Main-diff VCS selection continues to include a changed `EntryKind::Link`, proving scanner admission—not path hiding—is where the stable link is rejected.
- Real `review --target main`, `review --target dirty`, and `review --target file:...` regressions all omit the stable repository link path and external sentinel. `review --all` has the same content-visibility result for that fixture.
- A targeted stable-link rejection has a deterministic path-scoped diagnostic. Full traversal may silently omit a stable link, but reports an entry when a later observation detects that it changed after being seen as regular. Missing/deleted changed paths retain their current non-fatal skip semantics.
- Replacing a previously cached regular path with a stable link removes that key from a targeted cache and causes a `Hit`/`Wrote` cache report with no reuse or rescan of the link. A full scan likewise writes a replacement cache without the link while preserving/reusing unaffected regular files.
- `SCAN_CACHE_FORMAT_VERSION` is exactly one greater than the value present when this issue is implemented: baseline 2 becomes 3 only when no earlier cache-invalidating issue has landed. A handcrafted, stamp-matching entry using the immediately previous format and poisoned followed-link bytes is treated as a cache `Miss`, rescanned from the current regular file, rewritten in the current format, and never reused.
- Structured and serialized fixed-version caches contain neither the controlled fixture sentinel nor a `CachedFileEntry` for a rejected stable link.
- A path replaced by a link after inventory but before the next admission check is rejected by the scan attempt. Whenever post-open/post-read checks observe drift, the buffer is discarded before classification, hashing, splitting, result construction, or cache writing.
- Scanner content processing opens each uncached path once and uses that one byte buffer for file-type analysis, hashing, UTF-8 handling, and block splitting. Existing extension, canonical-name, NUL, and shebang behavior remains green.
- Unix external-sentinel tests are wholly `#[cfg(unix)]`; Windows-specific symlink setup is wholly `#[cfg(windows)]` and skips only unavailable/unauthorized setup. Production stable-link admission is enabled on every platform.
- No new dependency, compatibility shim, cache migration, or VCS-selection workaround is introduced; the deliberate version bump invalidates rather than migrates pre-fix caches.
- The documented invariant is scoped honestly: stable final/ancestor links are excluded, and observed ordinary drift is rejected. Adversarial concurrent pathname swapping is outside the containment guarantee.
- All focused commands, touched test targets, and final `just check` pass.

## Non-goals and risks

- Do not add general symlink-follow support, parse the target of a worktree link, or represent a worktree link as the external target file. The chosen policy is exclusion.
- Do not remove `EntryKind::Link` from changed-path selection. Link changes remain visible as selected paths and targeted diagnostics; scanner admission decides whether worktree bytes exist for review.
- Do not change historical revision scanning. Git tree readers consume stored object bytes and do not dereference the host filesystem; deciding whether Git link blobs should appear as reviewable text is separate work.
- Do not change `RepoPath`, target syntax, dirty/main semantics, feedback selection, ignore configuration, or subdirectory-root display semantics. They continue to feed normalized but untrusted paths into the scanner.
- Do not change the canonicalization policy for the scan root itself. Stable-state component admission begins below the already-established root/repository base.
- Do not solve this with only `canonicalize` plus a prefix check. That still follows links, permits stable in-root links contrary to the policy, and leaves the same ordinary check/read drift.
- Concurrent adversarial filesystem mutation is explicitly outside the containment guarantee. Component checks plus pre-open, post-open, and post-read stamp validation reject stable links and detected ordinary drift, but every pathname lookup remains raceable. Fully adversarial cross-platform containment would require retained root/directory handles, handle-relative no-follow opens, and platform file identities (`openat`/`O_NOFOLLOW`-style Unix handling and reparse-aware Windows APIs), which is intentionally outside this scoped defect and would require a separately justified dependency/design.
- Windows junctions and other non-symlink reparse-point policy must not be broadened casually in this fix. If the standard library reports such an entry as a symlink, it is rejected; a general reparse-point security model requires dedicated Windows coverage and is separate from the validated Unix symlink defect.
- Cache entries for stable, regular files that fail parsing or UTF-8 validation remain cacheable. Only admission/link/drift rejection is forced non-cacheable; broad cache error-policy changes are outside scope.

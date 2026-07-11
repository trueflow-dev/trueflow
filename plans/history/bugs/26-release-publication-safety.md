# Issue #26: Make release publication fail-safe and checksum-safe

Status: ready

Date: 2026-07-10

Baseline commit: 9a98914698c4

## Problem

The website and release scripts do not currently preserve ownership boundaries or treat a release as a validated, serialized publication unit:

- the root website mirror can delete every object under `download/`, even though those objects are owned by the release flow;
- the download preflight checks only that required filenames and some matching manifest fields exist, not that the manifest contains exactly the required rows or that their SHA-256 values match the archive bytes;
- the shared packager writes archive/checksum output directly to final paths and can retain stale same-target data;
- validation retains mutable source paths that another package run can replace before upload;
- current-object checks forget a published version after S3 cleanup creates delete markers;
- per-key conditionals do not serialize multi-key publication;
- deleting a specific current lock-object version in the versioned bucket can make an older `ACQUIRED`/`PREPARED` lock body current again;
- cleanup before the version-bearing website switch can delete the release still named by live installer/download links;
- releasing the publication lock before website sync lets a delayed older deploy overwrite the site after a newer release has completed;
- the full deploy mutates infrastructure and website content before packaging, smoke, or validation can fail.

The release flow must enforce these invariants:

1. `website/` owns ordinary site keys, but not `download/*` or `.trueflow-release/*`. Root website sync may delete obsolete website-owned keys and must never mutate either protected keyspace.
2. Before infrastructure/AWS activity, a private frozen snapshot contains exactly two required archives and one version-matched manifest. The manifest contains exactly two nonblank rows, one for each explicit required target, and no other row or filename.
3. Each row is `<64 lowercase hexadecimal SHA-256><whitespace><exact archive basename>`, has exactly two fields, and equals the frozen archive's actual SHA-256.
4. Full deploy smoke-tests and uploads the same frozen files. Mutation or replacement of original artifacts after validation cannot alter uploaded bytes.
5. A never-deleted ledger under `.trueflow-release/ledger/` permanently records the first archive/manifest digests for a version, so changed same-version bytes remain forbidden even after public delete markers.
6. A bucket-wide lock is a conditional state machine held continuously from the first remote publication operation through new archive/manifest commit, website sync and website invalidation, old-download cleanup, download invalidation, and ownership-checked release. Unlock verifies the current VersionId, ETag, nonce, and phase, then conditionally deletes the **current key without a version ID**, creating a current delete marker; it never deletes a phase version and cannot resurface an older lock body. A concurrent or delayed flow cannot switch website content out of order.
7. New and old complete releases coexist while the website changes from old version-bearing links to new links. Only a successful website sync **and** invalidation advances the lock to `WEBSITE_SWITCHED`; cleanup is forbidden in every earlier state.
8. Website failure or partial website mutation skips old-download cleanup. Because both releases remain downloadable, either old cached pages or newly written pages still reference valid artifacts.
9. Direct deploy has explicit phases: `begin` commits/verifies the new set and retains the old set while leaving the lock held; website switch advances the lock; `finalize` alone removes old downloads and only from `WEBSITE_SWITCHED`. There is no one-command/default destructive cleanup path.
10. Packaging promotes only complete temporary files, excludes the old same-target archive when regenerating the manifest, adds the replacement basename once, and rejects duplicate basenames.
11. Any failed or concurrent website/release deploy leaves at least one complete digest-consistent release public and never exposes a checksum-mismatched pair.

## Evidence

- `scripts/deploy-website.sh:15-21` runs `aws s3 sync website/ "s3://${SITE_BUCKET}/" --delete` at the bucket root and then invalidates `/*`. There is no filter excluding `download/*` or any future release-control keyspace.
- The AWS CLI `sync` reference says that `--delete` deletes files present at the destination but absent from the source, and explicitly says files excluded by filters are excluded from deletion. It defines `--exclude` as excluding matching files or objects from the command. Therefore `download/*`, which is absent from `website/`, is currently in the root deletion set; exact exclusions can preserve non-website keyspaces while retaining deletion of obsolete website-owned keys. See [AWS CLI `s3 sync` reference](https://docs.aws.amazon.com/cli/latest/reference/s3/sync.html#options).
- `scripts/deploy-downloads.sh:21-38` finds one checksum file and checks that each required archive exists and that `awk` sees at least one row whose second field is the archive name. It does not require exactly two records, reject an unsupported extra filename, count matching rows, constrain rows to two fields, validate a 64-character hexadecimal digest, or hash either archive.
- `scripts/deploy-downloads.sh:41-48` already calls `validate_artifact_dir` before resolving the bucket and distribution through the infrastructure CLI. The fix should preserve and strengthen this useful failure-before-side-effect boundary, then bind the validated bytes to a private snapshot rather than retaining mutable source paths.
- `scripts/deploy-downloads.sh:51-57` mirrors directly into `/download/` with `--delete` and invalidates only after the sync. If transfer and deletion interleave or two different versions sync concurrently, neither process establishes that a complete replacement remains public before obsolete keys disappear.
- `scripts/package-built-release.sh:44-58` already provides the portable repository convention for SHA-256 (`shasum -a 256`, falling back to `sha256sum`). `website/install.sh:49-75` uses the same tool preference, calculates the downloaded archive's actual digest, and compares it to the manifest-selected digest before extracting. The deploy preflight should use this existing dependency pattern and enforce the consumer's contract before publication.
- `scripts/package-built-release.sh:119-148` writes `tar` output directly to `ARCHIVE_PATH`, truncates the final checksum file with `: > "$CHECKSUM_NAME"`, and appends hashes to it. A failed `tar` can leave a partial final archive; a failed checksum command can replace a previously good manifest with an empty or partial file. A naive temporary-archive implementation would also enumerate the old same-target final archive and the new temporary archive into two rows for one final basename.
- `scripts/package-macos-release.sh:91-100` and `scripts/package-linux-release.sh:83-92` both delegate final archive and manifest creation to `package-built-release.sh`. Fixing the shared packager covers both entrypoints without a second convention.
- `scripts/smoke-test-release.sh:33-78` extracts an archive, checks the binary/README/LICENSE payload, runs `trueflow --version`, and exercises `trueflow review --all --json`. It is the existing behavioral smoke test the full deploy should run for the macOS archive it packages before any public mutation.
- `scripts/deploy-public-site.sh:109-147` currently runs `tofu init/fmt/validate/apply`, deploys `website/`, then packages macOS, and only then calls `deploy-downloads.sh`. A local artifact failure occurs after visible mutation. Conversely, merely moving download cleanup before the website would still be unsafe: `website/install.sh:5-7` and `website/install/index.html:73-89` carry a current version in installer defaults and concrete download links, so deleting the old release before those files switch creates live 404s.
- `website/README.md:24-34` assigns `/download/<artifact_name>` to release artifacts while the same bucket also serves the root website. `infra/terraform/main.tf:38-43` enables S3 versioning, which helps recovery but does not make a noncurrent object publicly downloadable.
- `infra/README.md:85-89` and `infra/terraform/README.md:127-129` document the current retention policy: `/download/` mirrors the supplied artifact directory and obsolete platform/version objects are deleted. This issue retains that public policy while moving deletion after successful commit; the new never-deleted control ledger is publication metadata, not a retained downloadable artifact.
- Amazon S3 documents that `PutObject` never creates a partial object and `If-None-Match: *` rejects an overwrite with `412 Precondition Failed`. It also documents the critical versioned-bucket exception: when the current version is a delete marker, `If-None-Match: *` may create a new current object even though older versions exist. Current-key immutability is therefore insufficient; a cleanup-excluded ledger is required. See [AWS CLI `s3api put-object` reference](https://docs.aws.amazon.com/cli/latest/reference/s3api/put-object.html) and [Amazon S3 conditional writes](https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html#conditional-error-response).
- S3 provides strong consistency and atomic updates for one key, but explicitly provides no atomic update across keys and tells applications to build their own locking when concurrent writers are a problem. Per-archive conditionals cannot serialize release-wide cleanup. See [Amazon S3 data consistency model](https://docs.aws.amazon.com/AmazonS3/latest/userguide/Welcome.html#ConsistencyModel).
- S3 also distinguishes simple delete from version-specific deletion: in a versioned bucket, deleting a key without `versionId` adds a current delete marker and makes ordinary GET/HEAD return 404, while deleting a specific version permanently removes it and can expose the next older version. Lock release must use the former, guarded by the current ETag, never the latter. See [Deleting object versions from a versioning-enabled bucket](https://docs.aws.amazon.com/AmazonS3/latest/userguide/DeletingObjectVersions.html#delete-request-use-cases).
- S3's lack of cross-key atomicity also means the website object set, manifest/archive objects, and cleanup cannot be one S3 transaction. The application-level lock must therefore span all three visible phases; releasing after download commit but before website sync permits a delayed older website write after a newer release.

## Reproduction

All automated reproduction uses local recording shims and a temporary versioned fake bucket; it never contacts AWS or OpenTofu.

1. Seed known-good `download/*` and `.trueflow-release/ledger/*` sentinels plus an obsolete website key. Current root `sync --delete` removes all of them because it has no protected-prefix exclusions.
2. Show current download validation accepts an archive mutated after manifest creation, duplicate rows, malformed rows, and two valid required rows plus an unsupported extra row, then reaches infra/AWS.
3. Inject tar/checksum failure and repackage a seeded same-target archive to reproduce partial/stale final output and the duplicate-logical-basename trap.
4. Replace original artifact/manifest paths after validation but before a recording `put-object` opens them to reproduce mutable-path upload.
5. In the versioned fake bucket publish v1, publish v2/delete public v1, then attempt changed v1. Current-only checks see delete markers as absence and permit reuse without an immutable ledger.
6. Start v1 and v2 publishers together. Without one lock, their distinct public keys both commit and their cleanup passes can delete each other's complete sets.
7. Seed website pages/install defaults that still name v1. Commit v2 downloads, delete v1, then run website sync: the live v1 links return 404 until the later switch. Reverse the race by delaying v1 website sync until after a v2 flow: without the same held lock, v1 content can overwrite the newer website.
8. Fail website sync at each copied website object and fail website invalidation after successful sync. Any protocol that already cleaned v1 leaves old cached/partial pages pointing at missing v1; safe behavior retains both v1 and v2 and skips cleanup.
9. Barrier two complete flows at `PREPARED`, before/after website sync, before cleanup, and before unlock. A v2 flow must not begin while v1 owns the lock, and a delayed v1 website/finalize call must fail after v1 unlocks.
10. Leave a lock as if killed. Age must not break it; wrong ETag/token recovery must fail.
11. Drive a lock through `ACQUIRED` and `PREPARED`, then delete the exact PREPARED VersionId. The fake versioned bucket resurfaces ACQUIRED as current, demonstrating why unlock/abort/recovery must not use `--version-id`.
12. Run full deploy with a failing smoke fixture and recording tofu/AWS shims. Current ordering records infrastructure/website mutation first.

These become initial red cases in `scripts/tests/release-publication-safety.sh`.

## Root cause

The bucket has multiple logical owners but root sync treats it as one namespace. `website/` owns neither public downloads nor publication-control metadata.

Release validation is syntactic and mutable-path based. It does not define an exact closed manifest set or bind uploaded bytes to the archive smoke-tested before AWS.

Packaging has no final-path commit point and does not explicitly model same-target replacement.

S3 current existence is not permanent publication history: cleanup delete markers make a historical key conditionally creatable again. Permanent immutability requires an independent never-deleted ledger.

S3 atomicity is per key, while a public release spans archives, manifest, version-bearing website files, invalidation, and deletion of the old set. Uploading new files first is insufficient if old files are deleted before every live/cached website view can safely select the new version. Likewise, a lock released before website sync does not serialize the actual public switch.

The correct transaction order is application-level: freeze/validate; acquire; reserve ledger; commit/verify new downloads while old remains; switch and invalidate website while both remain; clean old downloads; invalidate downloads; release. Failure before the website switch preserves old links; failure after a partial switch preserves both link targets.

## Implementation plan

1. **Add red behavioral shell tests first.**
   - Create `scripts/tests/release-publication-safety.sh` as POSIX `sh` with deterministic fixtures, temporary workspace, and fail-closed recording `aws`/`tofu` shims.
   - Model S3 versions/delete markers, conditional put/delete, object bodies/ETags/version IDs, strong current reads, root/download sync, and CloudFront invalidation. A simple conditional current-key delete creates a new current delete marker; version-specific deletion permanently removes only that version and reveals the next older version. Unknown shim calls fail.
   - Cover protected-prefix sentinels, exact two-row validation, malformed/extra/duplicate rows, mutated archives, atomic packaging failure, same-target repackage, source-path replacement, immutable ledger across public delete markers, and stale-lock recovery.
   - Add a website-version fixture helper that writes v1 or v2 consistently into `website/install.sh` defaults and every concrete archive/manifest link. Mismatched website/release versions fail locally before infra/AWS.
   - Add a website-failure matrix after new manifest commit: fail root sync before mutation, after each copied website object, and at website invalidation. Assert no old download gets a delete marker, both old/new manifests still resolve, cleanup/download invalidation do not run, and the owned lock is conditionally aborted or remains for fail-closed recovery.
   - Add a barrier-driven two-full-flow test. Pause v1 at `PREPARED`, after website sync, at `WEBSITE_SWITCHED`, and before unlock; attempt v2 at every barrier and require it to fail before ledger/public/website mutation. After v1 unlock, let v2 complete, then replay v1's old receipt and assert its website/finalize phases cannot run.
   - Add lock-version regressions: complete two consecutive successful full flows and a separate abort from `PREPARED`. After each unlock/abort, assert HEAD absence, a current delete marker, all phase bodies noncurrent, no recorded `delete-object --version-id`, and successful next `If-None-Match: *` acquisition without an older phase resurfacing. Include a negative fake-S3 control showing exact-version deletion would resurface the prior phase.
   - The successful order log must be: snapshot/smoke/local website-version gate; infra; lock acquire; ledger; new archives; new manifest; lock `PREPARED`; website sync; website invalidation; lock `WEBSITE_SWITCHED`; old-download cleanup; download invalidation; lock release. No cleanup may appear before `WEBSITE_SWITCHED`.

2. **Protect release-owned namespaces in website sync.**
   - Keep root `aws s3 sync ... --delete` in `scripts/deploy-website.sh`, adding exact `--exclude 'download/*'` and `--exclude '.trueflow-release/*'`. This preserves deletion only for website-owned keys.
   - Add `--publication-receipt RECEIPT` mode for a full release switch. Before sync, verify through `deploy-downloads.sh` that the receipt owns the current lock in `PREPARED` for the website's exact derived version. After successful sync and `/*` invalidation, atomically advance the lock/receipt to `WEBSITE_SWITCHED`. Do not release it.
   - Derive one website release version from `website/install.sh`'s `DEFAULT_VERSION` and every concrete versioned archive/manifest URL in `website/install/index.html`; require one consistent version. In receipt mode it must equal the frozen release version.
   - Preserve safe standalone website deploys by acquiring the same global lock, verifying the derived version's currently public archives/manifest against its immutable ledger, syncing/invalidation, then conditionally releasing. Standalone website deployment never performs download cleanup.
   - On sync/invalidation/state-transition failure, return nonzero without marking `WEBSITE_SWITCHED`. In a full flow the orchestrator runs receipt abort; old and new downloads remain.

3. **Make shared packaging atomic and same-target regeneration unambiguous.**
   - In `scripts/package-built-release.sh`, write archive and manifest temporaries inside `ARTIFACT_DIR`.
   - Enumerate complete existing version archives except `$ARCHIVE_NAME`, add the completed replacement once under its final basename, reject duplicate basenames, and hash final logical names.
   - Complete both temporaries before same-filesystem renames. Never truncate/append final paths. Trap failures; strict snapshot validation handles a crash between two final renames.
   - Keep macOS/Linux wrappers on this shared implementation.

4. **Create one strict private frozen snapshot and explicit deploy phases.**
   - Replace the unsafe ordinary destructive invocation with explicit modes in `scripts/deploy-downloads.sh`: `--validate-only ARTIFACT_DIR`; `--prepare-snapshot ARTIFACT_DIR SNAPSHOT_DIR`; `--begin-publication SNAPSHOT_DIR RECEIPT`; `--verify-publication-phase RECEIPT PHASE`; `--mark-website-switched RECEIPT`; `--finalize-publication SNAPSHOT_DIR RECEIPT`; and `--abort-publication RECEIPT`. The legacy one-directory form must fail with usage rather than upload+cleanup.
   - Prepare requires a new empty mode-0700 directory, selects exactly one manifest, derives exactly two required archive names, and copies only those three complete files. Validate snapshot exactness and two exact rows; calculate actual archive and manifest SHA-256. No original path is reopened afterward.
   - Begin revalidates the exact snapshot immediately before infra/AWS, uploads only snapshot paths, and writes a mode-0600 receipt atomically. Receipt contains no credentials: bucket/distribution, version, snapshot digests, owner nonce, lock key, current lock ETag/version, and phase.
   - Full deploy smoke-tests the same snapshot later passed to begin/finalize. A race hook replacing originals after first AWS activity cannot alter remote bodies.

5. **Use a never-deleted ledger and a conditional lock state machine.**
   - Reserve `.trueflow-release/ledger/$VERSION` and `.trueflow-release/publication.lock`, excluded from all cleanup/root sync.
   - `--begin-publication` makes lock acquisition the first S3 operation after local validation/infra output. Conditional create captures ETag/version; any existing/ambiguous lock fails closed.
   - Under the lock, compare/create canonical immutable ledger bytes containing version and SHA-256 of exactly two archives plus manifest. Never overwrite/delete ledger; changed historical bytes fail before public mutation.
   - Commit/verify both archives, then checksum-protected manifest, while every old download remains. Advance the lock body conditionally from `ACQUIRED` to `PREPARED`, update receipt ETag/phase atomically, and return with the lock intentionally held.
   - `--mark-website-switched` requires the receipt's current `PREPARED` nonce/ETag/version and conditionally updates the same lock key to `WEBSITE_SWITCHED` only after website sync and invalidation succeed.
   - Finalize requires current lock body, receipt, and snapshot to agree in `WEBSITE_SWITCHED`. A `PREPARED`, stale, replayed, missing, changed, or foreign receipt cannot delete anything.
   - Abort never cleans downloads. Immediately before unlock, HEAD/read the current lock and require its VersionId, ETag, nonce, and phase to match the receipt. Then issue conditional `delete-object` against the key/current ETag **without `--version-id`**, so S3 creates a current delete marker over all noncurrent phase bodies. Verify ordinary HEAD is absent. If ownership/state is ambiguous or the conditional fails, leave the lock for manual recovery.
   - Successful finalize uses the identical unlock helper after download invalidation. No production path may permanently delete the captured phase VersionId: that would reveal an older `ACQUIRED`/`PREPARED` version as current. The captured VersionId is verification evidence only.
   - No wall-clock stale timeout. Recovery inspects the current body/phase/ETag/VersionId and active processes; after proving abandonment, it conditionally deletes the current key using the observed ETag and **no version ID**, verifies a current delete marker/HEAD absence, and leaves all phase versions noncurrent. Wrong/replaced state survives.

6. **Finalize public retention only after the website switch.**
   - In begin, existing equal public objects are idempotent; different bytes fail. Missing archives use conditional `put-object`, each remotely rehashed. Manifest uploads last with AWS SHA-256 request checksum and remote byte/digest verification.
   - Begin performs no deletion and no download invalidation. At `PREPARED`, old and new complete releases intentionally coexist.
   - `deploy-website.sh --publication-receipt` performs root sync/invalidation under the held lock, with both versions present, then marks `WEBSITE_SWITCHED`.
   - Finalize re-verifies lock ownership/phase, immutable ledger, and the complete new remote set. Then run deletion-only `aws s3 sync EMPTY/ s3://.../download/ --delete` with exact exclusions for the new two archives and manifest. This preserves current-version retention while removing old downloads only after live website switch.
   - Invalidate `/download/*` and `/install.sh` while still locked, then call the shared verified current-key unlock (conditional ETag, no version ID) so a delete marker becomes current. Cleanup/download invalidation failure leaves website pointing to verified new files; website failure never enters finalize and preserves old files.
   - All cleanup helpers require `WEBSITE_SWITCHED` and current receipt ownership. There is no default/direct mode that can combine prepare and cleanup without the website transition.

7. **Reorder the full orchestrator as one locked public switch.**
   - `scripts/deploy-public-site.sh` order: package; create mode-0700 snapshot; strict validate; smoke snapshot macOS archive; validate website version equals snapshot version; tofu init/fmt/validate/apply; begin publication and receive `PREPARED`; website sync/invalidation with receipt and mark `WEBSITE_SWITCHED`; finalize old-download cleanup/download invalidation; unlock.
   - Install a trap after begin. Any failure before `WEBSITE_SWITCHED` invokes abort, never finalize. Failure after the website switch invokes a phase-aware safe exit: new release is verified and selected; cleanup may be retried under the still-owned lock or the lock is left for explicit recovery, never guessed.
   - A full flow never calls ordinary standalone website mode. Receipt mode prevents lock release between download commit and website switch, and stale receipt checks prevent delayed old website writes.
   - `--skip-website` cannot request destructive finalization: it may validate/begin a prepare-only publication that retains old downloads, then explicitly abort/releases, or skip download publication. `--skip-downloads` uses standalone locked website verification and never cleanup. Document/reject combinations that cannot satisfy a coherent switch.
   - `--skip-package`, `--skip-build`, `--skip-infra-apply`, and fast wrapper retain their intended local behavior without bypassing snapshot, phase, or lock gates.

8. **Run focused smoke before cleanup work.**
   - Run `sh scripts/tests/release-publication-safety.sh`. Require the website failure matrix, full-flow barriers, replay rejection, exact ordering, and all prior checksum/atomic/versioning tests green before docs/gate work.

9. **Finish docs and gate integration last.**
   - Add `test-release-publication` to `Justfile` and ordinary test/check once.
   - Update both infra READMEs and deploy help with exact begin → website switch → finalize/abort direct semantics. Explicitly warn that begin retains old objects and leaves a lock; finalize is legal only after successful receipt-bound website sync/invalidation; there is no destructive default.
   - Document standalone locked website deploy, phase-aware lock inspection/recovery, temporary old/new coexistence, and unchanged final current-version artifact retention.
   - Keep installer behavior unchanged; staging exercises its downstream digest check. Do not add source-text safety tests to the existing Rust landing-page test.

### Ordered affected files and symbols

1. `scripts/tests/release-publication-safety.sh` — versioned fake S3, website failure matrix, two-full-flow barriers/replay, prior safety cases.
2. `scripts/deploy-website.sh` — protected sync, website-version derivation, receipt-bound/standalone lock modes, website state transition.
3. `scripts/package-built-release.sh` — atomic same-target-safe regeneration.
4. `scripts/deploy-downloads.sh` — strict snapshot; begin/verify/mark/finalize/abort state machine; ledger/lock; upload and post-switch cleanup.
5. `scripts/deploy-public-site.sh` — snapshot/version gate, held-lock download→website→cleanup flow, phase-aware traps/skip flags.
6. `scripts/deploy-public-site-fast.sh` — accurate inherited phase ordering/help.
7. `Justfile` — focused recipe/gate.
8. `infra/README.md` — direct phases, failure recovery, unchanged final retention.
9. `infra/terraform/README.md` — matching operational contract.

## Verification and validation

Run from repository root:

```sh
sh scripts/tests/release-publication-safety.sh
cd trueflow && cargo test --test website_landing_page infra_terraform_skeleton_is_present_and_public_safe -- --exact
cd .. && just test-release-publication
just check
```

The shell harness is authoritative. It must prove original mutation cannot change snapshot uploads; invalid input has no infra/AWS side effects; root sync preserves protected sentinels; exact manifests/ledger survive delete markers; lock phases serialize two full flows; and cleanup never occurs before a receipt-bound successful website invalidation.

Manual staging validation:

1. Record all staging bucket versions and seed known-good v1 plus protected ledger and obsolete website sentinels.
2. Run website-only deploy and confirm protected keys/versions are untouched while obsolete website content is deleted.
3. Exercise validate-only malformed/mutated/duplicate/extra cases; confirm no infra/AWS.
4. Prepare a v2 snapshot and begin publication. Confirm v1 and v2 archives/manifests coexist, lock is `PREPARED`, v1 website links still work, and no delete marker/invalidation cleanup exists.
5. Fail staging website sync/invalidation. Run abort; confirm both versions still download and v1 cached/current links work. Do not inject destructive failure in production.
6. Repeat begin, successfully run receipt-bound website sync/invalidation, confirm lock becomes `WEBSITE_SWITCHED`, then finalize. Confirm v2 links/install succeed, only then v1 public keys receive delete markers, download invalidation runs, and lock releases.
7. Attempt finalize before website switch and replay the old receipt after a newer lock: both fail without cleanup or website mutation.
8. Publish v1, finish v2, then attempt changed v1; immutable ledger rejects it. Byte-identical restoration remains allowed through the full switch.
9. Simulate crashed locks in each phase. Verify age/wrong ETag cannot recover; after proving no owner, conditionally delete the current key without VersionId, confirm HEAD 404/current delete marker, and confirm no older phase resurfaces.
10. Complete two consecutive staging flows and one `PREPARED` abort. After each release inspect `list-object-versions`: phase versions remain noncurrent under a current delete marker, the next conditional acquisition succeeds, and ordinary HEAD never returns an older phase.
11. Run two full flows with staging only if safe harness coverage is insufficient. Confirm one lock owns download commit, website switch, cleanup, and invalidations end-to-end; no delayed older website sync succeeds.
12. Run `website/install.sh` through staging on both native platforms and confirm digest verification/install.

## Acceptance criteria

- Root website sync preserves `download/*` and `.trueflow-release/*` while deleting obsolete website-owned keys.
- Manifest is exactly two required well-formed rows; malformed, duplicate, missing, wrong, blank, and extra rows fail before infra/AWS.
- Full deploy smoke-tests and uploads one private exact three-file snapshot; original-path replacement cannot alter remote bytes.
- Packaging uses complete temporary files, excludes stale same-target archive, adds replacement once, and rejects duplicate basenames.
- Never-deleted per-version ledger rejects changed historical bytes across public delete markers.
- Conditional lock state machine is held across new download commit, website sync/invalidation, old-download cleanup, download invalidation, and release.
- At `PREPARED`, new and old complete releases coexist and no cleanup/download invalidation has run.
- Website mode verifies receipt ownership and matching version, then advances to `WEBSITE_SWITCHED` only after successful root sync and invalidation.
- Finalize rejects every phase except current owned `WEBSITE_SWITCHED`; abort never performs cleanup.
- Website sync/invalidation failure preserves old and new artifacts, skips cleanup, and leaves old cached/current links valid.
- Cleanup deletes obsolete downloads only after website switch, then invalidates downloads and unlocks; final public retention remains one current release.
- Concurrent full-flow barriers prove the loser cannot write downloads or website, and delayed/replayed older receipts cannot switch after a newer flow.
- Safe direct commands are explicit begin, receipt-bound website switch, finalize/abort phases. Legacy/default one-directory deployment cannot destructively sync.
- Build/smoke/validation failure has no infra/AWS; failed begin prevents website; no failed phase removes the last known-good release or publishes mismatch.
- Stale lock recovery is manual, exact, conditional, and fail-closed.
- Unlock, abort, and manual recovery verify current VersionId+ETag+nonce+phase, then conditionally delete the current lock key without `--version-id`; HEAD is absent behind a current delete marker and old phase versions never resurface.
- Two consecutive successful flows and a `PREPARED` abort prove the next `If-None-Match: *` acquisition succeeds after each delete marker.
- Network-free versioned fake-S3 harness includes website failure and concurrent full-flow matrices and runs in `just check`.
- Manual staging validates coexistence, switch ordering, failure preservation, finalize gate, immutable ledger, concurrency, and installer checksum behavior.
- Docs/help preserve final current-version retention and explain temporary transactional coexistence without treating it as historical retention.

## Non-goals and risks

- **No final public retention change.** `/download/` still ends with one supplied current-version set; old/new coexist only during a locked switch or failed prepare so live links remain valid.
- Never-deleted ledger/control metadata is outside downloadable-artifact retention.
- No migration, compatibility shim, legacy destructive path, force/checksum/lock bypass, auto stale timeout, or ledger rewrite.
- No Terraform, CloudFront policy, domain, artifact-name, installer CLI, target, or cross-platform execution redesign.
- Standalone website deploy is serialized and verifies its referenced release, but does not publish or clean downloads.
- A prepare reserves ledger/version and may leave verified unreferenced new objects after abort; same bytes retry or a new version is required. Later locked successful finalization removes obsolete public objects.
- A killed flow intentionally blocks publication until exact manual lock recovery. Recovery never permanently deletes a phase VersionId; it conditionally deletes the verified current key without a version argument so a delete marker hides all older lock states.
- Website sync can partially change files before failing; retaining both artifact sets is the safety mechanism for mixed old/new pages.
- Conditional single-part PutObject fits current archive size; future multipart replacement preserves request checksum, snapshot, ledger, lock phases, website-before-cleanup, and finalize gate.
- Atomic local rename is per file, not a multi-file transaction; strict frozen validation remains mandatory.
- Additional S3 reads/conditional permissions must be documented; 403/ambiguous responses never mean absent.

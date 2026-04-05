# Review Coverage Assessment and Implementation Plan

## Summary

The current Merkle tree abstraction is good at representing repository structure and hierarchical coverage inheritance, but it is not yet a good abstraction for answering rich review-coverage queries.

Today, the system is optimized for:

- building a hierarchical tree of directories, files, and semantic blocks
- answering a narrow question: "is this node covered by an approved review target?"
- filtering by a single check name when building a latest-verdict index

It is not optimized for:

- attaching all relevant reviews to a tree node or subtree
- answering direct vs inherited coverage separately
- aggregating by review check
- aggregating by identity
- querying subtree review quality
- expressing policy questions like "well-reviewed" in a reusable API

Recommendation:

- keep the existing tree as the structural backbone
- add a first-class coverage/read-model layer on top of `Tree + ReviewDatabase`
- make queries operate on resolved node associations and subtree aggregates instead of ad hoc record scans
- separate review facts from review policy

## Current state

### What is good today

1. The tree structure itself is clean and useful.
   - `TreeNode` / `TreeNodeId` / `TreeNodeKind` form a straightforward hierarchy in `trueflow/src/tree.rs`.
   - block nodes preserve parent relationships and nested containers
   - the tree already supports parent traversal and block lookup

2. The file and directory Merkle hashing model is coherent.
   - `FileState` computes a file `tree_hash` from child blocks in `trueflow/src/block.rs`
   - `TreeBuilder::compute_hashes` computes directory/tree hashes from sorted children in `trueflow/src/tree.rs`

3. Block lookup is reasonably precise.
   - `Tree::find_block_node(...)` uses `(path, block hash, start_line)` in `trueflow/src/tree.rs`
   - `ReviewIndex::block_verdict_for(...)` prefers exact `(hash, path, start_line)` before coarser fallbacks in `trueflow/src/store.rs`

4. Coverage inheritance exists.
   - `Tree::is_node_covered(...)` walks ancestors and accepts tree/file/block approvals in `trueflow/src/tree.rs`
   - this gives a usable "effective approval" model for simple review flows

### What is weak today

1. Reviews are not attached to the tree as a first-class structure.
   - records live only in `ReviewDatabase`
   - queries reconstruct associations ad hoc
   - there is no `node -> reviews` or `subtree -> review stats` interface

2. The current indexes are verdict-centric, not coverage-centric.
   - `ReviewIndex` stores latest verdict maps in `trueflow/src/store.rs`
   - `ApprovedTargets` stores only approvals
   - this is enough for `is approved?`, but not enough for richer questions

3. The main API loses information needed for analysis.
   - `approved_targets()` drops rejected/comment history and identity information
   - `latest_index(check_filter)` collapses history to one latest verdict per target
   - there is no reusable API for "all records that apply to this node"

4. Review association is duplicated and ad hoc.
   - `feedback` reconstructs block associations with `records_by_target_since(...)` plus `matching_reviews_for_block(...)`
   - coverage logic is spread across `review`, `feedback`, `tree`, and `store`

5. The tree has structure, but not subtree query indexes.
   - there is no DFS interval, descendant count cache, or subtree aggregate cache
   - "are the nested methods within this type reviewed?" currently requires custom traversal logic

6. The current target identity is not sufficient for file/tree precision.
   - block reviews have precise fallback using `path_hint` and `line_hint`
   - file and tree reviews are still keyed by hash only in `ReviewTargetRef`
   - identical files or identical directories can therefore alias at the approval layer

## What current queries are easy vs hard

### Easy or acceptable today

1. "Is this node effectively approved for `review`?"
   - supported by `ReviewIndex::is_block_approved(...)` plus `Tree::is_node_covered(...)`

2. "Is this block directly approved for a single check?"
   - supported if you know `(hash, path, start_line)` and build `latest_index(Some(check))`

3. "Does this file or directory inherit approval from an ancestor target?"
   - supported as a boolean through `Tree::is_node_covered(...)`

### Hard or awkward today

1. "Is this function well-reviewed?"
   - not directly supported
   - requires inventing policy atop a verdict-only API

2. "Are the AST sub-functions within this block well-reviewed?"
   - no reusable subtree coverage API
   - requires tree walking plus repeated block verdict checks

3. "Has this gotten a security review?"
   - possible only as a custom one-off using `latest_index(Some(&ReviewCheck::new(\"security\")?))`
   - not exposed as a node-centric query abstraction

4. "Has this been reviewed by at least two identities?"
   - not supported by any current index
   - requires scanning records and grouping by identity manually

5. "Show me all review evidence attached to this block or subtree."
   - not supported as a first-class query
   - only partially reconstructed in `feedback`

## Structural assessment

### Tree abstraction

The tree is structurally good, but semantically incomplete for review coverage.

Strengths:
- stable hierarchical structure for a single scan
- explicit parent/child relationships
- good enough block identity for a single snapshot

Gaps:
- no direct association of review records to nodes
- no subtree aggregate indexes
- no distinction between direct coverage and inherited coverage
- no first-class support for review facts beyond "approved"

Conclusion:
- keep the tree
- do not overload `Tree` itself with review semantics
- add a separate coverage index/read model built from the tree

### Store/index abstraction

The store layer is currently optimized for marking and yes/no approval checks, not analysis.

Strengths:
- append-only history
- typed `ReviewCheck`, `Verdict`, `Identity`
- precise block lookup fallback via `path_hint` and `line_hint`

Gaps:
- collapses too early to latest verdicts
- approval-only helper discards important information
- no node-resolved record binding layer
- file/tree identity precision is weaker than block precision

Conclusion:
- keep the raw record store
- stop using `ReviewIndex` as the main abstraction for higher-level coverage analysis
- add a richer analysis/read-model layer

## Design goal

After scanning and building the full tree, we should be able to resolve records to nodes and ask:

- direct review facts for a node
- effective review facts for a node including inherited approvals
- subtree review facts
- check-specific coverage
- identity-specific coverage
- threshold/policy questions like "well-reviewed"

The right abstraction is:

1. `Tree`
   Structural code graph for the current snapshot.

2. `ReviewDatabase`
   Raw historical records.

3. `CoverageIndex`
   Resolved association layer between tree nodes and review records, plus aggregates.

4. `CoveragePolicy`
   Policy layer defining what "well-reviewed" means.

## Proposed data model

### 1. Node locator / target resolution

Introduce an explicit resolved node locator concept for analysis:

```rust
enum NodeLocator {
    Root,
    Directory { path: RepoPath, hash: TreeHash },
    File { path: RepoPath, hash: TreeHash },
    Block { path: RepoPath, start_line: usize, hash: TreeHash },
}
```

This should be derived from the built tree, not stored as the main persisted record type initially.

Purpose:
- make all query code talk in terms of tree nodes / locators
- stop relying on ad hoc `(hash + path_hint + line_hint)` matching spread across call sites

### 2. Record binding

Add a resolution pass from records to node bindings:

```rust
struct RecordBinding {
    record_index: usize,
    node_id: TreeNodeId,
    relation: BindingRelation,
}

enum BindingRelation {
    Exact,
    PathScoped,
    HashOnly,
    InheritedFile,
    InheritedTree,
    Ambiguous,
    Unresolved,
}
```

Important:
- initial implementation can keep existing persistence and only build this at read time
- ambiguous bindings should be explicit, not silently flattened away

### 3. Coverage facts per node

Add a per-node facts structure:

```rust
struct NodeCoverageFacts {
    direct_records: Vec<RecordId>,
    inherited_records: Vec<RecordId>,
    direct_latest_by_check: HashMap<ReviewCheck, Verdict>,
    effective_latest_by_check: HashMap<ReviewCheck, Verdict>,
    direct_identities_by_check: HashMap<ReviewCheck, BTreeSet<IdentityKey>>,
    effective_identities_by_check: HashMap<ReviewCheck, BTreeSet<IdentityKey>>,
}
```

This should preserve enough information to answer:
- latest verdict by check
- count of distinct reviewers
- direct vs inherited review separately

### 4. Subtree aggregates

Add bottom-up subtree stats:

```rust
struct SubtreeCoverageStats {
    reviewable_block_count: usize,
    directly_approved_by_check: HashMap<ReviewCheck, usize>,
    effectively_approved_by_check: HashMap<ReviewCheck, usize>,
    distinct_identities_by_check: HashMap<ReviewCheck, BTreeSet<IdentityKey>>,
}
```

This allows:
- "what percent of nested methods have security review?"
- "is every descendant function covered?"
- "does this subtree have at least two distinct reviewers?"

## Proposed query API

Add a `coverage` module with an analysis-oriented API.

### Core constructor

```rust
let coverage = CoverageIndex::build(&tree, &database, CoverageBuildOptions::default())?;
```

### Core query entrypoints

```rust
coverage.node(node_id).direct_records()
coverage.node(node_id).effective_records()
coverage.node(node_id).latest_verdict_for(&check)
coverage.node(node_id).has_check(&check)
coverage.node(node_id).distinct_identities_for(&check)
coverage.node(node_id).is_well_reviewed(&policy)

coverage.subtree(node_id).stats()
coverage.subtree(node_id).all_blocks_satisfy(&policy)
coverage.subtree(node_id).reviewed_fraction(&check)
coverage.subtree(node_id).distinct_identity_count(&check)
```

### Policy layer

`well-reviewed` should not be hardcoded into the data structure.

Introduce a policy abstraction:

```rust
struct CoveragePolicy {
    required_checks: Vec<ReviewCheck>,
    min_distinct_identities: usize,
    require_latest_approval: bool,
    count_inherited_approval: bool,
}
```

Examples:
- general review policy
- security review policy
- "at least two reviewers" policy

## Algorithms

### Build phase

1. Build tree as today.
2. Assign DFS preorder / postorder intervals to every node.
3. Build locator indexes:
   - by exact block locator
   - by file path/hash
   - by directory path/hash
4. Resolve records to bound nodes.
5. Build per-node direct facts.
6. Propagate effective facts downward or compute them lazily with memoization.
7. Aggregate subtree stats bottom-up.

### Complexity target

Desired build complexity:
- `O(n + r + a)`

Where:
- `n` = number of tree nodes
- `r` = number of review records
- `a` = number of resolved record attachments

Desired query complexity:
- direct node queries: `O(1)` or `O(log k)`
- subtree summary queries: `O(1)` after precomputation
- descendant enumeration: `O(m)` for `m` nodes returned

## Recommended implementation phases

### Phase 1: assessment-backed read model

Goal:
- add a non-invasive coverage index without changing persisted record format

Tasks:
1. Add a `coverage` module.
2. Build `CoverageIndex` from `Tree + ReviewDatabase`.
3. Implement exact block record binding using current `path_hint` / `line_hint` semantics.
4. Implement file/tree binding using current target model plus path hints where available.
5. Add direct and effective verdict queries for a node.
6. Add distinct-identity counting by check.

Deliverable:
- internal API only
- no CLI surface change yet

### Phase 2: subtree coverage support

Goal:
- make container/subtree queries fast and reusable

Tasks:
1. Add DFS interval numbering to the tree or to coverage build state.
2. Add subtree stats precomputation.
3. Add subtree query APIs.
4. Add tests for nested type/function coverage.

Deliverable:
- `coverage.subtree(node_id)` query API

### Phase 3: policy layer

Goal:
- distinguish raw facts from product semantics like "well-reviewed"

Tasks:
1. Add `CoveragePolicy`.
2. Implement:
   - `is_well_reviewed(node, policy)`
   - `subtree_satisfies(node, policy)`
3. Decide whether inherited file/tree approvals count toward "well-reviewed" by default.

Deliverable:
- reusable policy-based query interface

### Phase 4: interface integration

Goal:
- expose the coverage model in tools people actually use

Candidate integrations:
1. `inspect`
   - show attached reviews and coverage stats for a block
2. `feedback`
   - switch from ad hoc matching to `CoverageIndex`
3. `review`
   - replace approval checks with coverage API where appropriate
4. future `coverage` command
   - query node/subtree status directly

Deliverable:
- one or two user-facing commands using the new coverage interface

### Phase 5: identity and target precision cleanup

Goal:
- fix remaining structural ambiguity in persisted review targets

Tasks:
1. Add location-aware precision for file/tree targets, not just blocks.
2. Decide whether to evolve record target identity to include path for file/tree reviews.
3. Add explicit handling for ambiguous bindings in the coverage layer.
4. Consider a versioned record migration only after read-model support is stable.

Deliverable:
- stronger target precision and less accidental aliasing

## Recommended tests

### Core binding tests

- exact block record binds to the correct node among duplicate-hash blocks
- path-scoped record binds to all matching coarse nodes only where intended
- hash-only record is marked ambiguous when multiple same-hash nodes exist
- file/tree records do not silently misbind across duplicate content/path scenarios

### Coverage semantics tests

- direct approval vs inherited approval are reported separately
- security review is queryable independently of general review
- distinct reviewer counts are correct
- rejected/comment history does not disappear from coverage facts

### Subtree tests

- container block subtree counts reflect nested methods/functions
- subtree reviewed fraction is correct for partially reviewed descendants
- "all nested functions reviewed" works for mixed nested coverage

### Policy tests

- "well-reviewed" for one-review policy
- "well-reviewed" for two-identity policy
- "well-reviewed" requiring security + review checks
- inherited-only approvals counted or excluded according to policy

## Open design questions

1. Should file/tree approvals count as "well-reviewed" for contained blocks, or only as inherited coverage?
   Recommendation:
   - yes for effective coverage
   - no for direct/well-reviewed unless policy explicitly allows it

2. Should repeated reviews by the same identity count more than once?
   Recommendation:
   - no for reviewer-count policies
   - yes for history views

3. Should the coverage layer expose only latest verdicts, or full timelines?
   Recommendation:
   - both:
     - fast latest/effective facts
     - access to raw bound records for audit/history views

4. Should ambiguity be treated as an error or as degraded coverage?
   Recommendation:
   - explicit degraded coverage with diagnostics
   - do not silently coerce ambiguous bindings into a single node

## Practical recommendation

Do not rewrite the store first.

Start with:
- a read-model layer that resolves current records onto the current tree
- a node/subtree coverage API
- policy helpers for common questions

Then, once that exists and is used by `feedback` / `inspect` / `review`, tighten persisted target identity where the read-model reveals ambiguity.

## Proposed first milestone

The first milestone should make the following possible in code:

```rust
let coverage = CoverageIndex::build(&tree, &database, Default::default())?;

let function = tree.find_block_node("src/lib.rs", &block).unwrap();

coverage.node(function).latest_verdict_for(&ReviewCheck::review());
coverage.node(function).latest_verdict_for(&ReviewCheck::new("security")?);
coverage.node(function).distinct_identity_count(&ReviewCheck::review());
coverage.subtree(function).all_blocks_approved_for(&ReviewCheck::review());
coverage.node(function).is_well_reviewed(&CoveragePolicy::two_person_review());
```

If the system can do that cleanly, the abstraction will be in the right place.

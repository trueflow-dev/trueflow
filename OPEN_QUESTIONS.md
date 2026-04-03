# Open Questions

## Churn suppression for code-language diffs: tokens vs tree-sitter

### Current decision boundary

For now, churn filtering should stay:

- **suppression only**, not a new review identity system
- **code languages only**, not markdown/text/docs
- **conservative by default**

That means any future token-based or tree-sitter-based comparison should decide
whether a changed block is **non-reviewable churn**, but should **not** become a
new public review target or a third persisted identity type.

### Why this is an explicit open question

Today the system already has more than one identity surface:

- content-addressed block/file/tree review targets
- diff fingerprints

Adding a new token hash or syntax-tree hash as another public identity without a
careful migration would increase complexity and confusion.

So the open question is:

> Should we use token normalization or tree-sitter structural hashing only as a
> churn-suppression signal for code-language diffs?

Current leaning:

- **yes** to suppression-only
- **no** to turning it into persisted review identity for now

### Relevant code pointers

#### Churn suppression entry point
- `trueflow/src/vcs.rs`
  - `block_has_changed_lines_in_diff`
  - `all_changed_lines_are_nonreviewable`
  - `is_trivial_closing_brace_addition`
  - `is_trivial_whitespace_only_change`
  - `is_trivial_formatting_only_replacement`

This is the current place where overlapping diff lines are classified as
reviewable vs non-reviewable churn.

#### Diff extraction for blocks
- `trueflow/src/vcs.rs`
  - `extract_block_diff_view_for_block`
  - `positioned_hunk_lines`

If suppression becomes more structural, this is still part of the path that maps
hunks onto block-local diff lines.

#### Language-aware block parsing
- `trueflow/src/block_splitter.rs`
  - `split`
  - language-specific tree-sitter query and parser setup

This is the main existing tree-sitter integration point for code languages.
If we reuse tree-sitter for churn suppression, we should probably compose with
this layer rather than invent an unrelated parser stack.

#### Language classification
- `trueflow/src/analysis.rs`
  - `Language`
  - `Language::from_extension`

This is where the code-language-only scope boundary is currently defined.

#### Review identity boundary
- `trueflow/src/store.rs`
  - `ReviewTargetKind`
  - `ReviewTargetRef`
  - `DiffFingerprint`

If we keep this work suppression-only, these types should probably remain
unchanged.

### Option A: token-based suppression

Compute a normalized token stream for a changed code block or replacement pair,
then suppress the diff if the normalized token streams are equal.

#### Pros
- simpler than full tree handling
- often faster and cheaper than parsing full syntax trees
- robust for many formatter-style changes
- easier to make conservative
- less coupled to exact parser tree shapes

#### Cons
- weaker structural guarantees
- tokenization quality varies by language/tooling
- harder to reason about nested structure-aware equivalence
- can become ad hoc if token normalization rules grow too much

#### Best fit
Good first step if we want:
- low risk
- conservative suppression
- fast iteration

### Option B: tree-sitter structural suppression

Parse code with tree-sitter, serialize a trivia-stripped structural form, and
compare hashes or canonical forms.

#### Pros
- stronger alignment with code structure
- easier to ignore trivia/comments/formatting in a principled way
- can support future structure-aware suppression decisions
- reuses the parser family we already depend on for block splitting

#### Cons
- more implementation complexity
- parser/query differences across languages can create subtle inconsistency
- tree serialization rules need careful definition
- malformed or partially parseable code may degrade behavior
- higher chance of accidentally creating a third identity model if we are not strict

#### Best fit
Good if we want:
- stronger long-term structure-aware churn suppression
- language-specific precision
- eventual reuse for richer review heuristics

### Recommended next-step order

1. keep expanding conservative line-level suppression first
   - EOF newline churn
   - CRLF/LF-only churn
   - broader delimiter-only churn
2. if that still leaves too much noise, try **token-based suppression** for code languages
3. only then consider **tree-sitter structural suppression**

### Non-goals for now

- no new persisted `syntax_hash` / `ast_hash` review target
- no replacement of `ReviewTargetKind`
- no markdown/text structural suppression in the first version
- no claim that syntax equality implies semantic equality

### Naming note

If we do eventually add a tree-based comparison helper, prefer names like:

- `syntax_hash`
- `structure_hash`
- `trivia_stripped_hash`

Avoid calling it a semantic hash unless we can actually defend that claim.

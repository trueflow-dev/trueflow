# Sub-Block Splitting and Implicit Review Plan

## 1. Core Architecture: "Implicit Composition"
We avoid complex schema changes by relying on deterministic splitting. If a Block `P` splits into `[S1, S2, S3]`, verifying `P` is equivalent to verifying all sub-blocks.

*   **Database**: Stores verdicts for sub-blocks (`S1`, `S2`) as standard rows.
*   **Scanner/Reviewer Logic**:
    1.  Check if Block `P` has a direct verdict. If yes, it is reviewed.
    2.  If no, **speculatively split** `P` using the default sub-splitting logic.
    3.  Check status of all resulting sub-blocks.
    4.  **Implicit Approval**: If all sub-blocks are approved, `P` is considered reviewed.
    5.  **Partial Review**: If some sub-blocks are approved, `P` remains "Unreviewed" but carries metadata indicating "Partial Progress".

## 2. Markdown Support (H1 Blocking)
*   **Update `block_splitter.rs`**:
    *   Initialize `tree-sitter-markdown` for `Language::Markdown`.
    *   **Rule**: Group content by Headers. A block starts at an `atx_heading` (H1-H6) and consumes nodes until the next header of the **same or higher level** (e.g., H1 consumes H2, but stops at next H1).
    *   *Fallback*: If no headers, the file is one block.

## 3. Sub-Splitting Logic (`src/sub_splitter.rs`)
*   **Function**: `pub fn split(block: &Block) -> Vec<Block>`
*   **Logic**:
    *   **Markdown**: Re-parse block content. Emit top-level children (`Paragraph`, `List`, `CodeBlock`) as blocks.
    *   **Code**: Split by `\n\n` (double newline regex).
        *   `kind`: `CodeParagraph`.

## 4. UI & UX Updates
*   **Metadata Header**:
    *   Add rich header to review views (CLI, Emacs, TUI).
    *   Format: `Reviewing... {block_type} {short_id} {changed_relative_time}`.
    *   Indicate "Partially Reviewed" status clearly (e.g., "Progress: 2/5 sub-blocks").
*   **CLI**:
    *   `trueflow inspect <hash> --split`: Outputs JSON list of sub-blocks.
*   **Emacs**:
    *   Bind `s` to `trueflow-split-block`.
    *   Replace section content with sub-blocks.
    *   Show status icons for already-reviewed sub-blocks.
*   **TUI**:
    *   Bind `s` to enter "Sub-Block View".

## 5. Testing Strategy (`tests/sub_block_coverage.rs`)
*   **Test Case 1: Partial Review**: Verify parent remains unreviewed if only one sub-block is marked.
*   **Test Case 2: Full Implicit Review**: Verify parent is filtered out/marked approved if all sub-blocks are marked.
*   **Test Case 3: Markdown**: Verify H1 grouping and paragraph splitting.

## 6. Dependencies
*   `tree-sitter-markdown`

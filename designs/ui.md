# Trueflow UX Manual

## Review Header
- Format: `{block_type} {block_name} in {path_from_root} (hash={shorthash}), subblocks:`
- Follow with a tree list of subblock kinds, showing the first 2 and last 2 when there are more than 4.
- Example:
  - `function main in src/main.rs (hash=a1b2c3d4), subblocks:`
  - `├─ CodeParagraph`
  - `├─ CodeParagraph`
  - `├─ ...`
  - `├─ CodeParagraph`
  - `└─ CodeParagraph`
- Use the repository root for `path_from_root`.
- `shorthash` should be the first 8 characters of the block hash.
- `block_name` should use semantic names when available (function/class name); fall back to line ranges.
- If the parent block is a function, label the first subblock as `Signature` when possible.

## Layout & Spacing
- Add extra vertical breathing room between the header line and the content body (2 blank lines).
- Actions should sit above the bottom edge by ~10% of the panel height (reserve a spacer area).
- Actions should be bolded for emphasis.
- Code view should sit on a very light grey panel to separate it from the background.

## Diff Highlighting
- Additions: green, deletions: red.
- Use a visible gutter marker (`+` or `-`) with an extra 2-4 spaces of padding to the left.
- Non-diff lines remain neutral/primary text color.

## TUI Preferences
- Center the review column at ~80 characters.
- Keep actions aligned within the centered column.
- Maintain the Gruvbox Light palette (background + neutral text).

## Emacs Preferences
- Focus buffer mirrors the review header format and spacing.
- Actions should be bold and visually separated near the bottom.
- Apply diff-style faces for additions/deletions in the focus view.

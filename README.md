# trueflow

![trueflow logo](./design/trueflow.jpg)

## Motivation

Reviewing is becoming the bottleneck to shipping.

Things that are now truer than ever:

- We cannot review all code.
- Existing code review tools are insufficient.
- Some code is more important to be reviewed than other code.
- We need tools to understand what has and has not been reviewed.

Some folks say that we won't need any code review, that agents will review
everything. I think it's true that agents will do a lot of the reviewing, maybe
even most of it, but I don't think we can eschew human review entirely, ever.
If we accept that bet as true, then it becomes imperative that we have the right
tools to review the right code, and to understand what has and hasn't been
reviewed.

## Design

Trueflow is a CLI-driven engine/tool for a highly efficient code review workflow.

It wants to be responsible for the following things:

- Presenting code in a highly reviewable format, optimized for efficiency.
- Ordering the review items in a optimal way.
- Keeping track of what was reviewed, by whom, and what lens it was reviewed from.

### Semantic code review

If you're familiar with git terminology, git has hunks. Hunks are like sections
of a file that have changed. In `trueflow`, we have `blocks`. `Blocks` are
semantic -- they aren't just text, but they have a `BlockType`. For example, in
a markdown document, we might show you `Paragraphs` as a primitive for review.
Similarly, for code, we might show you a changed or added `Function` or `Struct`
as a `Block` of review.

### Internal data structure: Merkle tree of blocks

A file creates the root of a merkle sub-tree of `blocks`. Each `Block` is a
[content-addressed](https://en.wikipedia.org/wiki/Content-addressable_storage)
hash of its canonicalized content. If you review a block, and then it gets
formatted, it's still marked as reviewed. Exceptional engineers ignore
whitespace changes in review diffs anyway; why not just canonicalize them out of
the review?

### Blocks are splittable into subblocks

If you're reviewing a `Block`, and it's too long to review,  you can `split` the
block (UX: press 's'). Then, we split the block into its constituent subblocks.

You can imagine that there's always a recursive relationship of `Blocks` to
sub-blocks.

For example, an example hierarchy of block types:

```
File
  ↘ 
   ImportBlock
  ↘ 
   Constant
   Constant
  ↘ 
   Function
    ↘ 
     FunctionSignature
     CodeParagraph
     CodeParagraph
     Comment
     CodeParagraph
     CodeParagraph
```

Imagine for example, the function in the above file is presented to you for
review, and it's just too long to digest at once. You can press 's' and
`trueflow` splits it into sub-blocks, and then you start reviewing one item at a
time. We show you the sub-blocks in order. Trueflow keeps track of what you've
reviewed, and you can comment independently on any block.

## UX

### TUI

We expose a TUI. It feeds you a stream of unreviewed blocks. You press a single
key to perform a review action on the block.

``` 
 'a' => approve the block
 'c' => comment on the block (feeds back into the agent)
 's' => split the block into sub-blocks, and recurse into them
 'q' => quit the review session (all progress is saved)
```

### Emacs package (magit-like)

``` 
 'a' => approve the block
 'c' => comment on the block (feeds back into the agent)
 's' => split the block into sub-blocks, and recurse into them
 'q' => quit the review session (all progress is saved)
```

## Feedback

After performing a review, all progress is saved to a database in a local file.
This file is an append-only database of `review` objects that point to the
block's sha256 w/ other metadata, including reviewer `identity`, and review
`label`. The idea behind labels is two fold:

1. We can have agent reviewers use this system, e.g. `code-simplifier` could be
   an agent label.
2. We can have specialized reviews, e.g. `security`, `legal`, `code`, `general`,
   `product`. Then you can understand the review posture of your code
   interdisciplinarily.

# Architecture

## Overview

The workspace follows a simple pipeline:

1. load configuration
2. build an indexing plan
3. collect file-level summaries
4. derive default review decisions
5. persist the review batch

## Components

### Configuration

The configuration layer keeps repository-specific defaults small and explicit. It
answers questions like:

- should generated files be indexed?
- how large should a review batch be?
- what repository name is embedded into metadata?

### Indexing

The indexing plan is intentionally deterministic. It resolves a short list of
roots and a short list of skip patterns. That makes it easier to compare two
runs and notice churn that comes from infrastructure rather than code changes.

### Review session

A review session translates indexing output into a small number of obvious next
steps. Large files tend to get comments. Smaller files tend to get approved.
Unsupported files are usually escalated for manual triage.

## Future work

- add dependency-aware prioritization
- add changed-lines-only presentation in the UI
- add structured export for automation

# Review Bench Workspace

This fixture models a small multi-language workspace that still feels realistic
for review indexing.

## Goals

- exercise Rust top-level item splitting
- exercise nested modules and structs
- exercise TypeScript, Python, shell, markdown, Nix, TOML, and Elisp parsing
- keep the repository large enough to make a Criterion benchmark useful

## Layout

- `src/` application and library logic
- `web/` frontend request helpers
- `python/` reporting utilities
- `scripts/` operator automation
- `docs/` architecture notes
- `nix/` developer shell setup
- `emacs/` editor integration helpers

## Review scenarios

1. configuration loading changes
2. indexing pipeline changes
3. review state persistence changes
4. API client request and retry changes
5. operator script safety changes

## Notes

The code here is intentionally plausible but not production-complete. It exists to
stress review indexing, not to build a real product.

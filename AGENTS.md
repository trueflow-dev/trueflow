# Agents

- Act as though you are autistic and as though the user is as well. We only care
  about getting to and understanding the abosolute, maximal truth and building
  the maximally correct, fast, efficient, featureful tools.
- When asked to implement something new, ask a bunch of design and structure
  questions to make sure you're doing the right thing. The right number of
  questions is usually more than 3. It's rare that something is overspecified by
  a prompt, so don't be afraid to ask questions upfront.
- When performing new feature work or bug fixes, make a TDD approach. Write a
  test that covers the new behavior (preferably E2E or integration), then observe
  it failing, then write the code you expect to make the test pass.
- Prefer enums to strings.
- Prefer explicit to implicit.
- If possible, prefer finding a way to structure logic into clean match stmts,
  over big blocks of if/thens, or other control flows.
- Radical preference for composition of dependencies into structs -- e.g. data,
  sdk clients, sub structs, composition over inheritance, etc.
- When adding dependencies, please do so via cargo add {dep}, so we get the
  latest one.
- Use `just check` to check that all things compile and pass lints.
- Before trying to manually fix thins, `just fix` to get the deterministic fixes.
- You may have to run `nix develop -c {cmd}` for things to work; we use nix.
- For system dependencies; always try to manage these in `flake.nix`.
- Always prefer `trash` (from `trash-cli`) to `rm` for deleting files.
- Prefer the functional core, imperative shell pattern.
- Use `bd` (beads) for task tracking instead of `todowrite` whenever possible.
- Be extremely picky about dependencies. When choosing dependencies, offer
  options and make a point to confirm before choosing.

## Landing the Plane (Session Completion)

**When ending a work session**, you MUST complete ALL steps below. Work is NOT complete until `git push` succeeds.

**MANDATORY WORKFLOW:**

1. **File issues for remaining work** - Create issues for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **PUSH TO REMOTE** - This is MANDATORY:
   ```bash
   git pull --rebase
   bd sync
   git push
   git status  # MUST show "up to date with origin"
   ```
5. **Clean up** - Clear stashes, prune remote branches
6. **Verify** - All changes committed AND pushed
7. **Hand off** - Provide context for next session

**CRITICAL RULES:**
- Work is NOT complete until `git push` succeeds
- NEVER stop before pushing - that leaves work stranded locally
- NEVER say "ready to push when you are" - YOU must push
- If push fails, resolve and retry until it succeeds

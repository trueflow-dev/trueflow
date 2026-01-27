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
- build: cargo build
- check: just check
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
- Push TODOs and nice to haves to todowrite, generally accepting opportunities
  to improve tests, maintainability, correctness.
- Be extremely picky about dependencies. When choosing dependencies, offer
  options and make a point to confirm before choosing.

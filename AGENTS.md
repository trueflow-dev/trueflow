# Agents

- When performing new feature work or bug fixes, make a TDD approach. Write a
  test that covers the new behavior (preferably E2E or integration), then observe
  it failing, then write the code you expect to make the test pass.
- Prefer enums to strings.
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
- Prefer the functional core, imperative shell pattern.

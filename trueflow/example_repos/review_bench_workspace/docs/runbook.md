# Runbook

## Daily startup

- enter the developer shell
- run the smoke workflow
- inspect the architecture notes
- open the dashboard and confirm the API is reachable

## Failure handling

If review indexing becomes suspiciously slow:

1. confirm the scan cache is being reused
2. inspect whether a large generated directory slipped into scope
3. compare the benchmark fixture against the real repository shape
4. rerun the cold and warm Criterion benchmarks

## Release checklist

- review recent persistence changes
- confirm CLI help still matches the workflows
- verify the benchmark fixture still looks realistic
- note any large diff-churn suppression changes in release notes

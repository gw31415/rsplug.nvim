# M0 local fixtures

The ignored M0 benchmarks create deterministic 128/512-repository fixtures in
a temporary directory. The fixture contains a dependency DAG, A/B revisions,
and local GraphQL/tarball response files. It is intentionally generated at
runtime so benchmark runs do not add repository data or contact GitHub.

Serve the generated directory with a localhost HTTP server when exercising
network adapters; no `.env` file is read by the harness.

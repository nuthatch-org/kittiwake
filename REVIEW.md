# REVIEW.md

Instructions for Claude's managed Code Review on pull requests in this repository. The local `/code-review`
command does not read this file; it reads `CLAUDE.md`. Put rules you want in both places in `CLAUDE.md`.

## What matters most here
kittiwake is the Rust service and cockpit behind Lodestar. Flag these as Important:
- **SQL and sqlx.** A changed or new `sqlx` query needs its `.sqlx/` cache regenerated (`deploy/sqlx-prepare.sh`). Schema changes in `db/schema.sql` need a migration path; flag any destructive change.
- **Gate query sets.** `nuthatch-gate`'s committed query sets must match the code. `yatr run --agent queries-check` is the test. Regenerate with `queries-write` and review the diff, never hand-edit.
- **SSRF and the edge.** Anything in `crates/ssrf` or `crates/edge` is security-sensitive. Any new outbound fetch must go through the ssrf guard. Logs must never carry secrets (`crates/edge/tests/no_secrets_in_logs.rs`).
- **Secrets and config.** Real values never appear in `deploy/*.toml`, scripts or tests. Only `*.example` files hold placeholders.
- **Deploy scripts.** Scripts in `deploy/` run on production hosts. Flag anything destructive without a guard, anything not idempotent, and anything that prints a secret.
- **Tests need no network or database.** A test that needs either is a finding.
- **RPC providers.** Only GraphOps or Alchemy, with the approved exceptions GME Chain (12821) and UnyKorn Record Chain (37332).
- **Tooling.** Clippy runs with warnings denied. `yatr run --agent ci` must pass.

## Calibration
- Missing test for a behaviour change: Important. Style: Nit, at most 3 per review.

## Skip
`.sqlx/*.json`, `Cargo.lock`, generated query sets.

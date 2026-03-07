# AGENTS.md

## Agent Workflow Rules

1. After every code change, run tests first.
- Required command: `cargo test`

2. Only if tests pass, build an executable.
- Required command: `cargo build --release`

3. If tests fail, do not build.
- Fix failures and rerun `cargo test` before building.

4. Report both outcomes in your final update.
- Include whether tests passed and whether the release build succeeded.

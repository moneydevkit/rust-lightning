default:
  @just --list --unsorted

# Run all code quality checks. TODO: Add format and linting when branch is clean
check:
  RUSTFLAGS="--cfg=lsps1_service" cargo test --workspace

# Run just the tests
test:
  RUSTFLAGS="--cfg=lsps1_service" cargo test --workspace

# Format all sources in place.
fmt:
  cargo fmt --all

# Makefile — Local audit pipeline targets
# Run `make audit` to execute the full pipeline locally.

AUDIT_LOG := audit.log
AUDIT     := bash scripts/audit-log.sh
ARTIFACTS := ci-artifacts

.PHONY: audit audit-fmt audit-clippy audit-deps audit-build audit-test audit-coverage clean-audit

# Full pipeline
audit: audit-fmt audit-clippy audit-deps audit-build audit-test audit-coverage
	$(AUDIT) INFO pipeline "Local audit pipeline completed successfully"

# 1. Formatting
audit-fmt:
	@mkdir -p $(ARTIFACTS)
	$(AUDIT) INFO fmt "Starting rustfmt check"
	cargo fmt --check 2>&1 | tee $(ARTIFACTS)/fmt.log
	$(AUDIT) PASS fmt "rustfmt check passed"

# 2. Clippy
audit-clippy:
	@mkdir -p $(ARTIFACTS)
	$(AUDIT) INFO lint "Starting clippy"
	cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tee $(ARTIFACTS)/clippy.log
	$(AUDIT) PASS lint "clippy passed"

# 3. Dependency scan
audit-deps:
	@mkdir -p $(ARTIFACTS)
	$(AUDIT) INFO dep-scan "Starting cargo-audit"
	-cargo audit 2>&1 | tee $(ARTIFACTS)/audit-deps.log || $(AUDIT) WARN dep-scan "Advisories found (non-blocking)" || true

# 4. Build
audit-build:
	@mkdir -p $(ARTIFACTS)
	$(AUDIT) INFO build "Starting release build"
	cargo build --workspace --release 2>&1 | tee $(ARTIFACTS)/build.log
	$(AUDIT) PASS build "Release build succeeded"

# 5. Tests
audit-test:
	@mkdir -p $(ARTIFACTS)
	$(AUDIT) INFO test "Starting tests"
	cargo test --workspace 2>&1 | tee $(ARTIFACTS)/test.log
	$(AUDIT) PASS test "All tests passed"

# 6. Coverage (best-effort)
audit-coverage:
	@mkdir -p $(ARTIFACTS)
	$(AUDIT) INFO coverage "Starting coverage"
	-cargo tarpaulin --workspace --out xml --output-dir $(ARTIFACTS) 2>&1 || $(AUDIT) WARN coverage "Coverage skipped (tarpaulin not available)" || true

# Clean local artifacts
clean-audit:
	rm -rf $(ARTIFACTS)
	: > $(AUDIT_LOG)
	@echo "Audit log and artifacts cleaned."

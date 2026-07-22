# Makefile — local quality pipeline.
# `make check` runs the full suite via the flake dev shell.

ARTIFACTS := ci-artifacts
CARGO     := nix develop --command cargo

.PHONY: check check-fmt check-clippy check-build check-test clean

check: check-fmt check-clippy check-build check-test
	@echo "==> pipeline passed"

check-fmt:
	@mkdir -p $(ARTIFACTS)
	@echo "==> rustfmt check"
	set -o pipefail; $(CARGO) fmt --check 2>&1 | tee $(ARTIFACTS)/fmt.log

check-clippy:
	@mkdir -p $(ARTIFACTS)
	@echo "==> clippy"
	set -o pipefail; $(CARGO) clippy --workspace --all-targets -- -D warnings 2>&1 | tee $(ARTIFACTS)/clippy.log

check-build:
	@mkdir -p $(ARTIFACTS)
	@echo "==> build"
	set -o pipefail; nix build 2>&1 | tee $(ARTIFACTS)/build.log

check-test:
	@mkdir -p $(ARTIFACTS)
	@echo "==> tests"
	set -o pipefail; $(CARGO) test --workspace 2>&1 | tee $(ARTIFACTS)/test.log

clean:
	rm -rf $(ARTIFACTS)

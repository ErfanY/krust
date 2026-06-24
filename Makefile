# krust Makefile — one entry point for local dev, CI, and release packaging.
# Targets mirror the commands in .github/workflows/{ci,release}.yml so `make` runs the
# exact same pipeline on your machine and on CI.
#
#   make            # list targets
#   make validate   # the full pre-PR / CI gate (fmt-check + check + test + release build)
#   make package    # build + tar a release artifact for the host (or TARGET=<triple>)
#   make lab-up lab-load lab-contexts   # stand up the kwok scale lab
#   make bench | soak | discover | metrics   # verification harnesses against the lab

# Resolve cargo: PATH first, then a Homebrew keg-only rustup install (macOS), else plain `cargo`.
CARGO ?= $(shell command -v cargo 2>/dev/null || command -v /opt/homebrew/opt/rustup/bin/cargo 2>/dev/null || echo cargo)
RUSTC ?= $(shell command -v rustc 2>/dev/null || command -v /opt/homebrew/opt/rustup/bin/rustc 2>/dev/null || echo rustc)
# Put cargo's directory on PATH so external subcommands (cargo-fmt, cargo-clippy) and rustc resolve
# even with a Homebrew keg-only rustup that isn't otherwise on PATH.
export PATH := $(dir $(CARGO)):$(PATH)

# Host target triple, used by `package` when TARGET is not supplied.
TARGET ?= $(shell $(RUSTC) -vV 2>/dev/null | sed -n 's/^host: //p')

# Defaults for the lab harness targets.
SCALE_KUBECONFIG ?= scripts/scale/kubeconfig-scale.yaml
KRUST_BIN := target/release/krust

.DEFAULT_GOAL := help

.PHONY: help fmt fmt-check check clippy test build build-release validate ci run clean \
        package lab-up lab-load lab-contexts lab-crds lab-churn lab-down \
        bench bench-contexts soak discover metrics

help: ## List available targets
	@awk 'BEGIN{FS=":.*?## "}/^[a-zA-Z0-9_-]+:.*?## /{printf "  \033[36m%-16s\033[0m %s\n",$$1,$$2}' $(MAKEFILE_LIST)

# ---- core dev pipeline (identical commands to CI) -------------------------------------
fmt: ## Format all code
	$(CARGO) fmt --all

fmt-check: ## Check formatting without writing
	$(CARGO) fmt --all -- --check

check: ## Type-check (locked)
	$(CARGO) check --locked

clippy: ## Lint with clippy (locked, warnings as errors) — not part of the CI gate
	$(CARGO) clippy --locked --all-targets -- -D warnings

test: ## Run the test suite (locked)
	$(CARGO) test --locked

build: ## Debug build
	$(CARGO) build --locked

build-release: ## Optimized release build
	$(CARGO) build --release --locked

validate: ## Full gate run sequentially: fmt-check -> check -> test -> release build
	$(MAKE) fmt-check
	$(MAKE) check
	$(MAKE) test
	$(MAKE) build-release

ci: validate ## Alias invoked by GitHub Actions

run: ## Run krust locally (pass args via ARGS="--context foo")
	$(CARGO) run -- $(ARGS)

clean: ## Remove build artifacts and generated dist/
	$(CARGO) clean
	rm -rf dist

# ---- release packaging (mirrors .github/workflows/release.yml) ------------------------
package: ## Build + tar a release artifact for TARGET (default: host triple)
	@test -n "$(TARGET)" || { echo "could not determine TARGET (rustc not found); pass TARGET=<triple>"; exit 1; }
	$(CARGO) build --release --locked --target $(TARGET)
	@PKG="krust-$(TARGET)"; \
	  rm -rf "dist/$$PKG"; mkdir -p "dist/$$PKG"; \
	  cp "target/$(TARGET)/release/krust" "dist/$$PKG/krust"; \
	  cp README.md LICENSE "dist/$$PKG/"; \
	  tar -C dist -czf "dist/$$PKG.tar.gz" "$$PKG"; \
	  echo "packaged dist/$$PKG.tar.gz"

# ---- kwok scale lab (scripts/scale) ---------------------------------------------------
lab-up: ## Create the kwok scale cluster
	scripts/scale/up.sh
lab-load: ## Load fake nodes/namespaces/pods (tune with KRUST_* env)
	scripts/scale/load.sh
lab-contexts: ## Generate the multi-context kubeconfig
	scripts/scale/contexts.sh
lab-crds: ## Install the sample CRD + custom resources
	scripts/scale/crds.sh
lab-churn: ## Run the churn generator (Ctrl-C to stop)
	scripts/scale/churn.sh
lab-down: ## Tear down the kwok cluster + generated kubeconfig
	scripts/scale/down.sh

# ---- verification harnesses (need the lab + a release build) --------------------------
bench: build-release ## Bench the hot paths against the lab
	$(KRUST_BIN) --kubeconfig $(SCALE_KUBECONFIG) --bench
bench-contexts: build-release ## Multi-context memory test (N=20 by default)
	$(KRUST_BIN) --kubeconfig $(SCALE_KUBECONFIG) --bench --bench-contexts $(or $(N),20)
soak: build-release ## Soak under churn (SECS=210 by default)
	$(KRUST_BIN) --kubeconfig $(SCALE_KUBECONFIG) --soak-secs $(or $(SECS),210)
discover: build-release ## Dynamic discovery catalog (RES=<plural> to also list one)
	$(KRUST_BIN) --kubeconfig $(SCALE_KUBECONFIG) --discover $(if $(RES),--discover-resource $(RES),)
metrics: build-release ## Live metrics probe (CTX=<context>, else the lab)
	$(KRUST_BIN) $(if $(CTX),--context $(CTX),--kubeconfig $(SCALE_KUBECONFIG)) --readonly --metrics

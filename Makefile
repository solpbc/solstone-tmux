# solstone-tmux Makefile
# Native tmux observer for solstone

SHELL := /bin/bash

.PHONY: all build hopper-install test test-only format ci clean install-service uninstall-service service-status service-logs package-linux release-linux validate-release sign-validate-release publish-release

APP := solstone-tmux
CARGO := cargo
CARGO_DENY_VERSION := cargo-deny 0.20.2
RUST_TARGETS := scripts/rust-targets.sh
RUST_GUARDS := scripts/check-rust-guards.sh
RELEASE_BIN := target/release/$(APP)
PACKAGE_FORMATS ?= tar,deb,rpm

all: build

build:
	$(CARGO) build --locked --workspace

hopper-install: build

test:
	$(CARGO) test --locked --workspace

test-only:
	@if [[ -z "$(TEST)" ]]; then \
		echo "Usage: make test-only TEST=<filter>"; \
		exit 1; \
	fi
	$(CARGO) test --locked -p $(APP) $(TEST)

format:
	$(CARGO) fmt --all

ci:
	@echo "Running CI checks..."
	@echo "=== Running repository guards (host execution) ==="
	@bash $(RUST_GUARDS)
	@echo ""
	@echo "=== Checking Rust formatting (host tool; no dependency resolution) ==="
	@$(CARGO) fmt --all --check
	@echo ""
	@echo "=== Running Rust clippy (host execution) ==="
	@$(CARGO) clippy --locked --workspace --all-targets -- -D warnings
	@echo ""
	@echo "=== Running Rust tests (host execution) ==="
	@$(CARGO) test --locked --workspace
	@echo ""
	@echo "=== Checking Rust dependency policy (host execution) ==="
	@actual="$$(cargo-deny --version 2>/dev/null || true)"; \
	if [[ "$$actual" != "$(CARGO_DENY_VERSION)" ]]; then \
		echo "cargo-deny 0.20.2 is required; found '$${actual:-not installed}'." >&2; \
		echo "Install cargo-deny 0.20.2 and ensure it is on PATH." >&2; \
		exit 1; \
	fi
	@cargo deny --offline --locked check licenses sources bans
	@targets="$$($(RUST_TARGETS))"; \
	echo "Rust dependency graph evidence (cargo deny --offline --locked with deny.toml [graph].targets; resolution only; no compile, link, runnable, or native-artifact claim):"; \
	while IFS= read -r target; do \
		echo "$$target: PASS — dependency graph resolves; resolution only; no compile, link, runnable, or native-artifact claim"; \
	done <<< "$$targets"
	@echo ""
	@echo "=== Checking Rust host compile ==="
	@set -o pipefail; \
	host="$$(rustc -vV | sed -n 's/^host: //p')"; \
	targets="$$($(RUST_TARGETS))"; \
	host_base="$$(echo "$$host" | cut -d- -f1-3)"; \
	host_configured=false; \
	while IFS= read -r target; do \
		target_base="$$(echo "$$target" | cut -d- -f1-3)"; \
		if [[ "$$target" == "$$host" || "$$target_base" == "$$host_base" ]]; then host_configured=true; fi; \
	done <<< "$$targets"; \
	if [[ "$$host_configured" != true ]]; then \
		echo "host cannot build any configured target: $$host" >&2; \
		exit 1; \
	fi; \
	echo "Rust host compile evidence (cargo check --locked --workspace --all-targets; host only; no executable linked; no native-artifact claim):"; \
	if $(CARGO) check --locked --workspace --all-targets; then \
		echo "$$host: PASS — host cargo check; no executable linked; no native-artifact claim"; \
	else \
		status=$$?; \
		echo "$$host: FAIL — host cargo check; no executable linked; no native-artifact claim"; \
		exit $$status; \
	fi
	@echo ""
	@echo "All CI checks passed!"

clean:
	$(CARGO) clean

install-service:
	$(CARGO) build --locked --release -p $(APP)
	$(CURDIR)/$(RELEASE_BIN) install-service

uninstall-service:
	$(CARGO) build --locked --release -p $(APP)
	$(CURDIR)/$(RELEASE_BIN) uninstall-service

service-status:
	$(CARGO) build --locked --release -p $(APP)
	$(CURDIR)/$(RELEASE_BIN) status

service-logs:
	@case "$$(uname -s)" in \
		Linux) journalctl --user -u solstone-tmux.service -n 100 --no-pager -f ;; \
		Darwin) log stream --style compact --predicate 'process == "solstone-tmux"' ;; \
		*) echo "service logs are supported only on Linux and macOS" >&2; exit 1 ;; \
	esac

package-linux:
	@test -n "$(RUST_TARGET)" || { echo "RUST_TARGET is required" >&2; exit 1; }
	@test -n "$(SOURCE_COMMIT)" || { echo "SOURCE_COMMIT is required" >&2; exit 1; }
	@test -n "$(OUTPUT_DIRECTORY)" || { echo "OUTPUT_DIRECTORY is required" >&2; exit 1; }
	@case "$(RUST_TARGET)" in \
		*-musl) builder="zigbuild";; \
		*) builder="build";; \
	esac; \
	SOLSTONE_TMUX_SOURCE_COMMIT="$(SOURCE_COMMIT)" \
		$(CARGO) $$builder --locked --release --target "$(RUST_TARGET)"
	packaging/linux/build-candidate.sh \
		"$(RUST_TARGET)" \
		"$(SOURCE_COMMIT)" \
		"$(CURDIR)/target/$(RUST_TARGET)/release/$(APP)" \
		"$(OUTPUT_DIRECTORY)" \
		"$(PACKAGE_FORMATS)"

release-linux:
	@test -n "$(SOURCE_COMMIT)" || { echo "SOURCE_COMMIT is required" >&2; exit 1; }
	@test -n "$(OUTPUT_DIRECTORY)" || { echo "OUTPUT_DIRECTORY is required" >&2; exit 1; }
	packaging/linux/build-release-lane.sh \
		"$(SOURCE_COMMIT)" \
		"$(OUTPUT_DIRECTORY)"

validate-release:
	@test -n "$(CANDIDATE_DIRECTORY)" || { echo "CANDIDATE_DIRECTORY is required" >&2; exit 1; }
	SOLSTONE_TMUX_TEST_COMPLETE_CANDIDATE="$(CANDIDATE_DIRECTORY)" \
		$(CARGO) test --locked -p $(APP) --test release_validator \
			validates_real_complete_set_when_requested -- --exact

sign-validate-release:
	@test -n "$(SOURCE_COMMIT)" || { echo "SOURCE_COMMIT is required" >&2; exit 1; }
	@test -n "$(CANDIDATE_DIRECTORY)" || { echo "CANDIDATE_DIRECTORY is required" >&2; exit 1; }
	@test -n "$(MINISIGN_SECRET_KEY)" || { echo "MINISIGN_SECRET_KEY is required" >&2; exit 1; }
	@test -n "$(SIGNED_CANDIDATE_DIRECTORY)" || { echo "SIGNED_CANDIDATE_DIRECTORY is required" >&2; exit 1; }
	packaging/publish-release.sh --sign-and-validate-only \
		"$(SOURCE_COMMIT)" \
		"$(CANDIDATE_DIRECTORY)" \
		"$(MINISIGN_SECRET_KEY)" \
		"$(SIGNED_CANDIDATE_DIRECTORY)"

publish-release:
	@test -n "$(SOURCE_COMMIT)" || { echo "SOURCE_COMMIT is required" >&2; exit 1; }
	@test -n "$(CANDIDATE_DIRECTORY)" || { echo "CANDIDATE_DIRECTORY is required" >&2; exit 1; }
	@test -n "$(MINISIGN_SECRET_KEY)" || { echo "MINISIGN_SECRET_KEY is required" >&2; exit 1; }
	packaging/publish-release.sh \
		"$(SOURCE_COMMIT)" \
		"$(CANDIDATE_DIRECTORY)" \
		"$(MINISIGN_SECRET_KEY)"

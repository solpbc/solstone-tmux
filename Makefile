# solstone-tmux Makefile
# Standalone tmux terminal observer for solstone

.PHONY: install test test-only format ci clean clean-install uninstall deploy upgrade service-restart service-status service-logs uninstall-service

# Service deployment
APP := solstone-tmux
PIPX_FLAGS :=
UNIT := solstone-tmux.service

# Default target
all: install

# Virtual environment directory
VENV := .venv
VENV_BIN := $(VENV)/bin
PYTHON := $(VENV_BIN)/python

# Require uv
UV := $(shell command -v uv 2>/dev/null)
ifndef UV
$(error uv is not installed. Install it: curl -LsSf https://astral.sh/uv/install.sh | sh)
endif

# Venv tool shortcuts
PYTEST := $(VENV_BIN)/pytest
RUFF := $(VENV_BIN)/ruff

# Marker file to track installation
.installed: pyproject.toml
	@echo "Installing solstone-tmux with uv..."
	$(UV) venv --quiet --allow-existing $(VENV)
	$(UV) pip install --quiet -e ".[dev]" --python $(PYTHON)
	@touch .installed

# Install package in editable mode with dev dependencies
install: .installed

# Run all tests
test: .installed
	@echo "Running tests..."
	$(PYTEST) tests/ -q

# Run a specific test file or pattern
test-only: .installed
	@if [ -z "$(TEST)" ]; then \
		echo "Usage: make test-only TEST=<test_file_or_pattern>"; \
		echo "Example: make test-only TEST=tests/test_capture.py"; \
		echo "Example: make test-only TEST=\"-k test_function_name\""; \
		exit 1; \
	fi
	$(PYTEST) $(TEST)

# Auto-format and fix code, then report remaining issues
format: .installed
	@echo "Formatting with ruff..."
	@$(RUFF) format .
	@$(RUFF) check --fix .
	@echo ""
	@echo "Checking for remaining issues..."
	@$(RUFF) check . || { echo ""; echo "Issues above need manual fixes."; exit 1; }
	@echo ""
	@echo "All clean!"

# Run CI checks (what CI would run)
ci: .installed
	@echo "Running CI checks..."
	@echo "=== Checking formatting ==="
	@$(RUFF) format --check . || { echo "Run 'make format' to fix formatting"; exit 1; }
	@echo ""
	@echo "=== Running ruff ==="
	@$(RUFF) check . || { echo "Run 'make format' to auto-fix"; exit 1; }
	@echo ""
	@echo "=== Running tests ==="
	@$(MAKE) test
	@echo ""
	@echo "All CI checks passed!"

deploy:
	@command -v pipx >/dev/null 2>&1 || { echo "pipx not found. Install with: sudo dnf install pipx  (or apt install pipx)"; exit 1; }
	@echo "==> Installing $(APP) with pipx"
	pipx install --force $(PIPX_FLAGS) .
	# Never use editable pipx installs here — they couple the running service
	# to the working tree; `git checkout` silently downgrades the deployed version.
	@echo "==> Installing systemd user unit"
	$(APP) install-service
	@echo "==> Service status"
	systemctl --user --no-pager status $(UNIT) | head

upgrade: ci
	@command -v pipx >/dev/null 2>&1 || { echo "pipx not found. Install with: sudo dnf install pipx  (or apt install pipx)"; exit 1; }
	@echo "==> Upgrading $(APP) with pipx"
	pipx install --force $(PIPX_FLAGS) .
	@echo "==> Reloading systemd and restarting $(UNIT)"
	systemctl --user daemon-reload
	systemctl --user restart $(UNIT)
	systemctl --user --no-pager status $(UNIT) | head

service-restart:
	systemctl --user restart $(UNIT)

service-status:
	systemctl --user --no-pager status $(UNIT)

service-logs:
	journalctl --user -u $(APP) -n 100 --no-pager -f

uninstall-service:
	-systemctl --user disable --now $(UNIT)
	-rm -f $$HOME/.config/systemd/user/$(UNIT)
	-systemctl --user daemon-reload
	-pipx uninstall $(APP)

# Clean build artifacts and caches
clean:
	@echo "Cleaning build artifacts..."
	rm -rf build/ dist/ *.egg-info/ src/*.egg-info/
	rm -rf .pytest_cache/ .mypy_cache/ .ruff_cache/
	find . -type d -name "__pycache__" -exec rm -rf {} + 2>/dev/null || true
	find . -type f -name "*.pyc" -delete 2>/dev/null || true
	rm -f .installed

# Remove venv and all artifacts
uninstall: clean
	@echo "Removing virtual environment..."
	rm -rf $(VENV)

# Clean everything and reinstall
clean-install: uninstall install

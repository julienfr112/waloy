# Waloy Makefile

.PHONY: all build test unit-test integration-test bench clean format lint garage-download garage-clean

all: build

build:
	cargo build --release

test: unit-test integration-test bench

unit-test:
	cargo test --lib

clean:
	cargo clean

# Format and lint
format:
	cargo fmt

lint:
	cargo clippy

bench:
	cargo bench --features full

# ---------------------------------------------------------------------------
# Garage (S3-compatible storage) for integration tests
# ---------------------------------------------------------------------------
GARAGE_VERSION        ?= v2.2.0
GARAGE_DIR            ?= garage
GARAGE_BIN            = $(GARAGE_DIR)/garage
GARAGE_CONF           = $(GARAGE_DIR)/config/garage.toml
GARAGE_RELEASES_BASE  = https://garagehq.deuxfleurs.fr/_releases
GARAGE_OS             ?= $(shell uname -s)
GARAGE_ARCH           ?= $(shell uname -m)

ifeq ($(GARAGE_ARCH),x86_64)
GARAGE_TARGET ?= x86_64-unknown-linux-musl
else ifeq ($(GARAGE_ARCH),aarch64)
GARAGE_TARGET ?= aarch64-unknown-linux-musl
else ifeq ($(GARAGE_ARCH),arm64)
GARAGE_TARGET ?= aarch64-unknown-linux-musl
else ifeq ($(GARAGE_ARCH),armv6l)
GARAGE_TARGET ?= armv6l-unknown-linux-musleabihf
else ifeq ($(GARAGE_ARCH),i686)
GARAGE_TARGET ?= i686-unknown-linux-musl
endif

garage-download:
	@mkdir -p $(GARAGE_DIR)
	@if [ "$(GARAGE_OS)" != "Linux" ]; then \
		echo "Unsupported OS: $(GARAGE_OS). Linux only."; exit 1; \
	fi
	@if [ -z "$(GARAGE_TARGET)" ]; then \
		echo "Unsupported arch: $(GARAGE_ARCH)."; exit 1; \
	fi
	@if [ ! -x $(GARAGE_BIN) ] || ! $(GARAGE_BIN) --version >/dev/null 2>&1; then \
		echo "Downloading Garage $(GARAGE_VERSION)..."; \
		curl -fL "$(GARAGE_RELEASES_BASE)/$(GARAGE_VERSION)/$(GARAGE_TARGET)/garage" \
			-o $(GARAGE_BIN) && chmod +x $(GARAGE_BIN); \
	fi

garage-clean:
	rm -rf $(GARAGE_DIR)/data $(GARAGE_DIR)/metadata $(GARAGE_DIR)/config

# ---------------------------------------------------------------------------
# integration-test: start Garage, run tests, kill Garage on exit (trap)
# ---------------------------------------------------------------------------
GARAGE_ABS_DIR = $(CURDIR)/$(GARAGE_DIR)
GARAGE_CLI     = $(GARAGE_ABS_DIR)/garage -c $(GARAGE_ABS_DIR)/config/garage.toml

integration-test: garage-download
	@rm -rf $(GARAGE_DIR)/data $(GARAGE_DIR)/metadata $(GARAGE_DIR)/config
	@mkdir -p $(GARAGE_DIR)/config $(GARAGE_DIR)/data $(GARAGE_DIR)/metadata
	@RPC_SECRET=$$(openssl rand -hex 32); \
	printf '%s\n' \
		"rpc_secret = \"$$RPC_SECRET\"" \
		'rpc_bind_addr = "0.0.0.0:3901"' \
		"metadata_dir = \"$(GARAGE_ABS_DIR)/metadata\"" \
		"data_dir = \"$(GARAGE_ABS_DIR)/data\"" \
		'db_engine = "lmdb"' \
		'replication_factor = 1' \
		'consistency_mode = "dangerous"' \
		'' \
		'[s3_api]' \
		'api_bind_addr = "0.0.0.0:3900"' \
		's3_region = "garage"' \
		'' \
		'[admin]' \
		'api_bind_addr = "0.0.0.0:3903"' \
		'admin_token = "test-admin-token"' \
		> $(GARAGE_CONF)
	@echo "Starting Garage..."
	@$(GARAGE_ABS_DIR)/garage -c $(GARAGE_ABS_DIR)/config/garage.toml server \
		>$(GARAGE_ABS_DIR)/garage.log 2>&1 & GARAGE_PID=$$!; \
	trap "echo 'Stopping Garage...'; kill $$GARAGE_PID 2>/dev/null; wait $$GARAGE_PID 2>/dev/null" EXIT; \
	echo "Garage PID: $$GARAGE_PID"; \
	sleep 3; \
	echo "Waiting for Garage to be ready..."; \
	for i in $$(seq 1 30); do \
		if $(GARAGE_CLI) status >/dev/null 2>&1; then break; fi; \
		if [ "$$i" -eq 30 ]; then echo "Garage failed to start (see garage/garage.log)"; exit 1; fi; \
		sleep 1; \
	done; \
	echo "Configuring Garage..."; \
	NODE_ID=$$($(GARAGE_CLI) node id -q 2>/dev/null); \
	$(GARAGE_CLI) layout assign -z dc1 -c 1G "$$NODE_ID"; \
	$(GARAGE_CLI) layout apply --version 1 2>/dev/null || true; \
	$(GARAGE_CLI) bucket create waloy-test 2>/dev/null || true; \
	$(GARAGE_CLI) key delete --yes waloy-test-key 2>/dev/null || true; \
	KEY_OUTPUT=$$($(GARAGE_CLI) key create waloy-test-key 2>&1); \
	ACCESS_KEY=$$(echo "$$KEY_OUTPUT" | grep "Key ID" | awk '{print $$NF}'); \
	SECRET_KEY=$$(echo "$$KEY_OUTPUT" | grep "Secret key" | awk '{print $$NF}'); \
	$(GARAGE_CLI) bucket allow --read --write --owner waloy-test --key waloy-test-key; \
	echo "Garage ready: access_key=$$ACCESS_KEY"; \
	WALOY_TEST_S3_ENDPOINT=http://localhost:3900 \
	WALOY_TEST_S3_REGION=garage \
	WALOY_TEST_S3_BUCKET=waloy-test \
	WALOY_TEST_S3_ACCESS_KEY=$$ACCESS_KEY \
	WALOY_TEST_S3_SECRET_KEY=$$SECRET_KEY \
	cargo test --test integration --test integration_axum -- --nocapture && \
	echo "Running feature-gated integration tests..." && \
	WALOY_TEST_S3_ENDPOINT=http://localhost:3900 \
	WALOY_TEST_S3_REGION=garage \
	WALOY_TEST_S3_BUCKET=waloy-test \
	WALOY_TEST_S3_ACCESS_KEY=$$ACCESS_KEY \
	WALOY_TEST_S3_SECRET_KEY=$$SECRET_KEY \
	cargo test --test integration --features compression,encryption -- --nocapture && \
	echo "Running CLI integration tests..." && \
	WALOY_TEST_S3_ENDPOINT=http://localhost:3900 \
	WALOY_TEST_S3_REGION=garage \
	WALOY_TEST_S3_BUCKET=waloy-test \
	WALOY_TEST_S3_ACCESS_KEY=$$ACCESS_KEY \
	WALOY_TEST_S3_SECRET_KEY=$$SECRET_KEY \
	cargo test --test integration_cli --features cli -- --nocapture && \
	echo "Running performance integration tests..." && \
	WALOY_TEST_S3_ENDPOINT=http://localhost:3900 \
	WALOY_TEST_S3_REGION=garage \
	WALOY_TEST_S3_BUCKET=waloy-test \
	WALOY_TEST_S3_ACCESS_KEY=$$ACCESS_KEY \
	WALOY_TEST_S3_SECRET_KEY=$$SECRET_KEY \
	cargo test --test integration_perf -- --nocapture

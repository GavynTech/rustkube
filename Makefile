SHELL := /bin/bash
API_URL := http://127.0.0.1:6443

.PHONY: build release fmt fmt-check clippy test e2e dev-up dev-down clean

build:
	cargo build --workspace

release:
	cargo build --workspace --release

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

clippy:
	cargo clippy --workspace --all-targets -- -D warnings

test:
	cargo test --workspace

e2e:
	./scripts/e2e.sh

# Dev cluster on the workstation = the unprivileged control plane: the API
# server and the scheduler. The node agent needs root + real namespaces, so
# it runs on the Ubuntu VM via scripts/phase3-demo.sh and phase4-demo.sh; the
# controllers join dev-up in Phase 5.
dev-up: build
	@mkdir -p .dev
	@if [ -f .dev/api.pid ] && kill -0 $$(cat .dev/api.pid) 2>/dev/null; then \
		echo "rk-api already running (pid $$(cat .dev/api.pid))"; \
	else \
		RUST_LOG=$${RUST_LOG:-info} nohup ./target/debug/rk-api > .dev/api.log 2>&1 & echo $$! > .dev/api.pid; \
		for i in $$(seq 1 40); do \
			curl -fsS $(API_URL)/healthz > /dev/null 2>&1 && break; \
			sleep 0.25; \
		done; \
		curl -fsS $(API_URL)/healthz > /dev/null || { echo "rk-api failed to start; see .dev/api.log"; exit 1; }; \
		echo "rk-api up at $(API_URL) (pid $$(cat .dev/api.pid), log .dev/api.log)"; \
	fi
	@if [ -f .dev/scheduler.pid ] && kill -0 $$(cat .dev/scheduler.pid) 2>/dev/null; then \
		echo "rk-scheduler already running (pid $$(cat .dev/scheduler.pid))"; \
	else \
		RUST_LOG=$${RUST_LOG:-info} nohup ./target/debug/rk-scheduler > .dev/scheduler.log 2>&1 & echo $$! > .dev/scheduler.pid; \
		echo "rk-scheduler up (pid $$(cat .dev/scheduler.pid), log .dev/scheduler.log)"; \
	fi

dev-down:
	@stopped=0; \
	for c in scheduler api; do \
		if [ -f .dev/$$c.pid ]; then \
			kill $$(cat .dev/$$c.pid) 2>/dev/null || true; \
			rm -f .dev/$$c.pid; \
			echo "stopped rk-$$c"; stopped=1; \
		fi; \
	done; \
	[ $$stopped -eq 1 ] || echo "dev cluster is not running"

clean:
	cargo clean
	rm -rf .dev

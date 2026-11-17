.PHONY: frontend build run test test-sanitizer lint format verify css-determinism bench seed backup clean

# Rust toolchain. Set CARGO=+nightly to use the nightly toolchain for test-sanitizer.
CARGO ?= cargo
PNPM ?= pnpm
PNPM_STORE ?= $(CURDIR)/.pnpm-store

# Generated design-system stylesheet embedded by crates/shorten-web.
CSS_TARGET := crates/shorten-web/assets/styles.css

BENCH_TARGET ?= http://127.0.0.1:8080/00000000
BENCH_DURATION ?= 60s
BENCH_CONCURRENCY ?= 200

SEED_DATABASE ?= .data/benchmark.db
SEED_COUNT ?= 1000000
SEED_BATCH ?= 1000

BACKUP_DATABASE ?= .data/shorten.db
BACKUP_OUTPUT ?= .data/backups/shorten.db

# Rebuild the Astro + shadcn design system into the embedded stylesheet.
# Only needed when the design system changes: the generated CSS is committed.
frontend:
	$(PNPM) -C frontend install --store-dir $(PNPM_STORE)
	$(PNPM) -C frontend run build

build:
	$(CARGO) build --workspace

run:
	$(CARGO) run -p shorten-server --bin shorten

test:
	$(CARGO) test --workspace

# Rust has no built-in race detector. Thread sanitizer is the nightly
# equivalent and is optional: it needs a nightly toolchain, a specific
# target and a C compiler for the sanitizer runtime.
#
#   RUSTFLAGS="-Zsanitizer=thread" $(CARGO) +nightly test \
#     -Zbuild-std --target x86_64-unknown-linux-gnu --workspace
test-sanitizer:
	@echo "optional: RUSTFLAGS=\"-Zsanitizer=thread\" $(CARGO) +nightly test -Zbuild-std --target x86_64-unknown-linux-gnu --workspace"

lint:
	$(CARGO) clippy --workspace --all-targets -- -D warnings

format:
	$(CARGO) fmt --all

verify:
	$(CARGO) fmt --all -- --check
	$(CARGO) clippy --workspace --all-targets -- -D warnings
	$(CARGO) test --workspace
	$(MAKE) css-determinism

# Rebuild the design system and prove the committed stylesheet is the exact
# artifact the build produces. Byte comparison, so it works outside git too.
css-determinism:
	@tmpdir=$$(mktemp -d); \
	cp $(CSS_TARGET) $$tmpdir/committed.css; \
	$(PNPM) -C frontend install --store-dir $(PNPM_STORE) >/dev/null; \
	$(PNPM) -C frontend run build >/dev/null; \
	if cmp -s $$tmpdir/committed.css $(CSS_TARGET); then \
		echo "css-determinism: styles.css matches the design-system build"; \
	else \
		echo "css-determinism: styles.css is stale — run 'make frontend' and commit the result" >&2; \
		rm -rf $$tmpdir; exit 1; \
	fi; \
	rm -rf $$tmpdir

bench:
	$(CARGO) run --release -p shorten-cli --bin bench -- \
		-target $(BENCH_TARGET) \
		-duration $(BENCH_DURATION) \
		-concurrency $(BENCH_CONCURRENCY)

seed:
	$(CARGO) run --release -p shorten-cli --bin seed -- \
		-database $(SEED_DATABASE) \
		-count $(SEED_COUNT) \
		-batch $(SEED_BATCH)

backup:
	$(CARGO) run -p shorten-cli --bin backup -- \
		-database $(BACKUP_DATABASE) \
		-output $(BACKUP_OUTPUT)

clean:
	$(CARGO) clean
	rm -rf .pnpm-store

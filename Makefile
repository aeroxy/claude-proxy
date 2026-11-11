.PHONY: build check run test clean bump-patch bump-minor bump-major update-formula release release-linux release-all

LINUX_TARGET := x86_64-unknown-linux-musl
LINUX_OUT    := target/$(LINUX_TARGET)/release

## Build the project (debug)
build:
	cargo build

## Release build — use this before manual testing
release:
	cargo build --release
	zip -j target/release/claude-proxy-macos-arm64.zip target/release/claude-proxy

## Linux x86_64 release: a static musl binary, cross-compiled with Zig as the C compiler
## One-time setup: brew install zig && cargo install cargo-zigbuild
##                 rustup target add x86_64-unknown-linux-musl
release-linux:
	cargo zigbuild --release --target $(LINUX_TARGET)
	zip -j $(LINUX_OUT)/claude-proxy-linux-x86_64.zip $(LINUX_OUT)/claude-proxy

## Both release zips (what `make update-formula` hashes)
release-all: release release-linux

## Type-check without producing a binary
check:
	cargo check

## Run the CLI; pass subcommands via ARGS
##   make run ARGS="start"
##   make run ARGS="stop"
run:
	cargo run -- $(ARGS)

## Run tests
test:
	cargo test

## Remove build artifacts
clean:
	cargo clean

## Bump the patch version (0.8.1 → 0.8.2) and update all version references
bump-patch:
	@old=$$(grep '^version' Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/'); \
	major=$$(echo $$old | cut -d. -f1); \
	minor=$$(echo $$old | cut -d. -f2); \
	patch=$$(echo $$old | cut -d. -f3); \
	new="$$major.$$minor.$$((patch+1))"; \
	sed -i '' "s/^version = \"$$old\"/version = \"$$new\"/" Cargo.toml; \
	sed -i '' "s/version \"$$old\"/version \"$$new\"/" Formula/claude-proxy.rb; \
	sed -i '' "s|/$$old/|/$$new/|g" Formula/claude-proxy.rb; \
	echo "$$old → $$new"

## Bump the minor version (0.8.1 → 0.9.0) and update all version references
bump-minor:
	@old=$$(grep '^version' Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/'); \
	major=$$(echo $$old | cut -d. -f1); \
	minor=$$(echo $$old | cut -d. -f2); \
	new="$$major.$$((minor+1)).0"; \
	sed -i '' "s/^version = \"$$old\"/version = \"$$new\"/" Cargo.toml; \
	sed -i '' "s/version \"$$old\"/version \"$$new\"/" Formula/claude-proxy.rb; \
	sed -i '' "s|/$$old/|/$$new/|g" Formula/claude-proxy.rb; \
	echo "$$old → $$new"

## Bump the major version (0.8.1 → 1.0.0) and update all version references
bump-major:
	@old=$$(grep '^version' Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/'); \
	major=$$(echo $$old | cut -d. -f1); \
	new="$$((major+1)).0.0"; \
	sed -i '' "s/^version = \"$$old\"/version = \"$$new\"/" Cargo.toml; \
	sed -i '' "s/version \"$$old\"/version \"$$new\"/" Formula/claude-proxy.rb; \
	sed -i '' "s|/$$old/|/$$new/|g" Formula/claude-proxy.rb; \
	echo "$$old → $$new"

## Update both SHA256s in Formula/claude-proxy.rb from the local release zips
## (run after `make release-all`, before uploading the zips)
##   make update-formula
update-formula:
	@mac_zip="target/release/claude-proxy-macos-arm64.zip"; \
	linux_zip="$(LINUX_OUT)/claude-proxy-linux-x86_64.zip"; \
	for f in "$$mac_zip" "$$linux_zip"; do \
		test -f "$$f" || { echo "$$f not found — run make release-all first"; exit 1; }; \
	done; \
	mac_sha=$$(shasum -a 256 "$$mac_zip" | cut -d' ' -f1); \
	linux_sha=$$(shasum -a 256 "$$linux_zip" | cut -d' ' -f1); \
	echo "macOS SHA256: $$mac_sha"; \
	echo "Linux SHA256: $$linux_sha"; \
	sed -i '' "/claude-proxy-macos-arm64.zip/,/sha256/ s/sha256 \"[a-f0-9]*\"/sha256 \"$$mac_sha\"/" Formula/claude-proxy.rb; \
	sed -i '' "/claude-proxy-linux-x86_64.zip/,/sha256/ s/sha256 \"[a-f0-9]*\"/sha256 \"$$linux_sha\"/" Formula/claude-proxy.rb; \
	echo "Formula/claude-proxy.rb updated"

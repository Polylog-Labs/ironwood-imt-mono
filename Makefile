.PHONY: run build web-deps

run: web-deps
	cargo run --release --locked --bin ironwood-indexer -- --public-rpc

build: web-deps
	cargo build --release --locked --bin ironwood-indexer

web-deps:
	@if [ ! -f node_modules/typescript/bin/tsc ] || \
		[ ! -f node_modules/@types/node/package.json ] || \
		[ ! -f node_modules/.package-lock.json ] || \
		[ package.json -nt node_modules/.package-lock.json ] || \
		[ package-lock.json -nt node_modules/.package-lock.json ]; then \
		npm ci --include=dev --ignore-scripts; \
	fi

.PHONY: run

run:
	cargo run --release --locked --bin ironwood-indexer -- --public-rpc

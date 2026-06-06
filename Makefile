.PHONY: build install run clean

build:
	cargo build --release

install: build
	mkdir -p ~/.local/bin
	install -m 755 target/release/clear-notification-daemon ~/.local/bin/clear-notification-daemon

run:
	cargo run

clean:
	cargo clean

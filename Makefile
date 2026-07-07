.PHONY: build install run clean

build:
	cargo build --release

install: build
	mkdir -p ~/.local/bin
	install -m 755 ../target/release/cce-notifier ~/.local/bin/cce-notifier
	mkdir -p ~/.config/systemd/user
	install -m 644 cce-notifier.service ~/.config/systemd/user/cce-notifier.service

run:
	cargo run

clean:
	cargo clean

# Zig 0.15 (used to build libghostty-vt) can't compile against the macOS 27
# SDK headers; tools/xcrun-shim points the build at an older SDK instead.
export PATH := $(CURDIR)/tools/xcrun-shim:$(PATH)

.PHONY: build release test install clean

build:
	cargo build

release:
	cargo build --release

test:
	cargo test

install:
	cargo install --path . --locked

clean:
	cargo clean

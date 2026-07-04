.PHONY: build install clean

BIN := abtop
BINDIR ?= $(HOME)/.cargo/bin
TARGET := target/release/$(BIN)

build:
	cargo build --release
	@if [ "$$(uname -s)" = "Darwin" ]; then \
		codesign --force --sign - "$(TARGET)"; \
	fi

install: build
	mkdir -p "$(BINDIR)"
	install -m 0755 "$(TARGET)" "$(BINDIR)/$(BIN)"

clean:
	cargo clean

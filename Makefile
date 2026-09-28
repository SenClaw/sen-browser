ID := sen-browser
DEV_PORT := 4970
CARGO_BUILD_JOBS ?= 4
export CARGO_BUILD_JOBS

VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
TARGET_DIR := $(or $(CARGO_TARGET_DIR),target)

UNAME_S := $(shell uname -s)
UNAME_M := $(shell uname -m)
ifeq ($(UNAME_S),Darwin)
  ifeq ($(UNAME_M),arm64)
    PLATFORM := darwin-arm64
  else
    PLATFORM := darwin-x64
  endif
else ifeq ($(UNAME_S),Linux)
  ifeq ($(UNAME_M),aarch64)
    PLATFORM := linux-arm64
  else
    PLATFORM := linux-x64
  endif
else
  PLATFORM := windows-x64
endif

ifeq ($(PLATFORM),windows-x64)
  EXE := .exe
else
  EXE :=
endif

SHA256 := $(shell command -v sha256sum >/dev/null 2>&1 && echo sha256sum || echo shasum -a 256)

DIST := dist
PKG_NAME := $(ID)-$(VERSION)-$(PLATFORM)
PKG_DIR := $(DIST)/$(PKG_NAME)

.PHONY: build test package install-local run-dev scripts clean

build:
	cargo build --release

# Unit tests always; real-browser tests run when Chrome is installed.
test:
	cargo test

# dist/<id>-<version>-<platform>.tar.gz (+ .sha256), manifest beside bin/ —
# see senclaw/docs/runtime-protocol.md §2.3.
package: build
	rm -rf "$(PKG_DIR)"
	mkdir -p "$(PKG_DIR)/bin"
	cp "$(TARGET_DIR)/release/$(ID)$(EXE)" "$(PKG_DIR)/bin/$(ID)$(EXE)"
	cp senclaw-runtime.json "$(PKG_DIR)/senclaw-runtime.json"
	cd "$(DIST)" && tar czf "$(PKG_NAME).tar.gz" -C "$(PKG_NAME)" .
	cd "$(DIST)" && $(SHA256) "$(PKG_NAME).tar.gz" > "$(PKG_NAME).tar.gz.sha256"
	@echo "packaged $(DIST)/$(PKG_NAME).tar.gz"

# Through `senclaw runtime install-local` (honours HOME, so run it with a
# scratch HOME while developing — never against a live ~/.senclaw).
install-local: package
	senclaw runtime install-local "$(DIST)/$(PKG_NAME).tar.gz"

# Standalone serve on a fixed dev port: no token (no auth), no parent (no watchdog).
run-dev:
	env -u SENCLAW_RUNTIME_TOKEN -u SENCLAW_PARENT_PID cargo run --release -- serve --host 127.0.0.1 --port $(DEV_PORT)

# The approved script bundle, for the extension build.
scripts:
	cargo run --quiet -- scripts

clean:
	cargo clean -p $(ID)
	rm -rf $(DIST)

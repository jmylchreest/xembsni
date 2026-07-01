# xembsni — build & install helpers.
#
# User install (default; for niri/Hyprland user systemd service):
#     make && make install
#     systemctl --user enable --now xembsni.service
#
# System install:
#     make && sudo make install PREFIX=/usr/local

CARGO ?= cargo
PREFIX ?= $(HOME)/.local
BINDIR ?= $(PREFIX)/bin
# User systemd unit dir by default; override for a system unit, e.g.
#   make install UNITDIR=/usr/lib/systemd/user
UNITDIR ?= $(HOME)/.config/systemd/user

BIN := target/release/xembsni

.PHONY: all build release test check fmt clippy install uninstall clean

all: build

build release: ## Build the release binary
	$(CARGO) build --release --bin xembsni

test: ## Run the test suite under a headless X server + D-Bus session
	xvfb-run -a dbus-run-session -- $(CARGO) test --workspace

check: fmt clippy ## Formatting + lint checks

fmt:
	$(CARGO) fmt --all --check

clippy:
	$(CARGO) clippy --workspace --all-targets

install: release ## Install the binary and systemd user unit
	install -Dm755 $(BIN) $(DESTDIR)$(BINDIR)/xembsni
	install -Dm644 contrib/systemd/xembsni.service $(DESTDIR)$(UNITDIR)/xembsni.service
	@echo "Installed. Enable with: systemctl --user enable --now xembsni.service"

uninstall: ## Remove installed files
	rm -f $(DESTDIR)$(BINDIR)/xembsni
	rm -f $(DESTDIR)$(UNITDIR)/xembsni.service

clean:
	$(CARGO) clean

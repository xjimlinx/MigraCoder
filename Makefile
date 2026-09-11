SHELL := /bin/sh

CARGO ?= cargo
PREFIX ?= $(HOME)/.local
DESTDIR ?=
BINDIR := $(abspath $(PREFIX))/bin

TARGET_DIR := target
RELEASE_BIN := $(TARGET_DIR)/release/migracoder
GUI_BIN := $(TARGET_DIR)/release/migracoder-gui
COMPLETION_DIR := build/completions
INSTALL_BIN := $(DESTDIR)$(PREFIX)/bin/migracoder
INSTALL_GUI_BIN := $(DESTDIR)$(PREFIX)/bin/migracoder-gui
DESKTOP_FILE := assets/io.github.migracoder.MigraCoder.desktop
ICON_FILE := assets/io.github.migracoder.MigraCoder.svg
INSTALL_DESKTOP_FILE := $(DESTDIR)$(PREFIX)/share/applications/io.github.migracoder.MigraCoder.desktop
INSTALL_ICON_FILE := $(DESTDIR)$(PREFIX)/share/icons/hicolor/scalable/apps/io.github.migracoder.MigraCoder.svg

.PHONY: all build release test lint check completions install install-cli install-completions \
	install-fish install-bash install-zsh gui run-gui install-gui install-all uninstall clean help

all: build

build:
	$(CARGO) build --locked

release:
	$(CARGO) build --release --locked

test:
	$(CARGO) test --locked

lint:
	$(CARGO) fmt --check
	$(CARGO) clippy --all-targets --locked -- -D warnings

check: lint test

gui run-gui:
	$(CARGO) run --release --bin migracoder-gui

completions: \
	$(COMPLETION_DIR)/migracoder.fish \
	$(COMPLETION_DIR)/migracoder.bash \
	$(COMPLETION_DIR)/_migracoder \
	$(COMPLETION_DIR)/migracoder.nu \
	$(COMPLETION_DIR)/migracoder.elv \
	$(COMPLETION_DIR)/migracoder.ps1

$(COMPLETION_DIR):
	mkdir -p $@

$(COMPLETION_DIR)/migracoder.fish: release | $(COMPLETION_DIR)
	$(RELEASE_BIN) completions fish > $@

$(COMPLETION_DIR)/migracoder.bash: release | $(COMPLETION_DIR)
	$(RELEASE_BIN) completions bash > $@

$(COMPLETION_DIR)/_migracoder: release | $(COMPLETION_DIR)
	$(RELEASE_BIN) completions zsh > $@

$(COMPLETION_DIR)/migracoder.nu: release | $(COMPLETION_DIR)
	$(RELEASE_BIN) completions nushell > $@

$(COMPLETION_DIR)/migracoder.elv: release | $(COMPLETION_DIR)
	$(RELEASE_BIN) completions elvish > $@

$(COMPLETION_DIR)/migracoder.ps1: release | $(COMPLETION_DIR)
	$(RELEASE_BIN) completions powershell > $@

install-cli: release
	install -Dm755 $(RELEASE_BIN) $(INSTALL_BIN)
	@printf 'installed: %s\n' '$(INSTALL_BIN)'

install-gui: release
	install -Dm755 $(GUI_BIN) $(INSTALL_GUI_BIN)
	install -Dm644 $(DESKTOP_FILE) $(INSTALL_DESKTOP_FILE)
	sed -i 's|^Exec=.*|Exec=$(BINDIR)/migracoder-gui|' $(INSTALL_DESKTOP_FILE)
	install -Dm644 $(ICON_FILE) $(INSTALL_ICON_FILE)
	@printf 'installed: %s\n' '$(INSTALL_GUI_BIN)'
	@printf 'desktop entry: %s\n' '$(INSTALL_DESKTOP_FILE)'

install: install-cli install-gui

install-all: install install-fish

install-fish: install-cli $(COMPLETION_DIR)/migracoder.fish
	install -Dm644 $(COMPLETION_DIR)/migracoder.fish \
		$(DESTDIR)$(HOME)/.config/fish/completions/migracoder.fish

install-bash: install-cli $(COMPLETION_DIR)/migracoder.bash
	install -Dm644 $(COMPLETION_DIR)/migracoder.bash \
		$(DESTDIR)$(PREFIX)/share/bash-completion/completions/migracoder

install-zsh: install-cli $(COMPLETION_DIR)/_migracoder
	install -Dm644 $(COMPLETION_DIR)/_migracoder \
		$(DESTDIR)$(PREFIX)/share/zsh/site-functions/_migracoder

install-completions: install-fish install-bash install-zsh

uninstall:
	$(RM) $(INSTALL_BIN)
	$(RM) $(INSTALL_GUI_BIN)
	$(RM) $(INSTALL_DESKTOP_FILE)
	$(RM) $(INSTALL_ICON_FILE)
	$(RM) $(DESTDIR)$(HOME)/.config/fish/completions/migracoder.fish
	$(RM) $(DESTDIR)$(PREFIX)/share/bash-completion/completions/migracoder
	$(RM) $(DESTDIR)$(PREFIX)/share/zsh/site-functions/_migracoder

clean:
	$(CARGO) clean
	$(RM) -r $(COMPLETION_DIR)

help:
	@printf '%s\n' \
		'make build                构建 debug 版本' \
		'make release              构建 release 版本' \
		'make check                运行格式、Clippy 和测试' \
		'make completions          生成六种 Shell 补全' \
		'make gui                  启动原生图形界面' \
		'make install              安装 CLI、GUI 和桌面启动项' \
		'make install-cli          只安装 CLI 到 ~/.local/bin' \
		'make install-gui          安装 GUI 和桌面启动项' \
		'make install-all          安装 CLI、GUI 和 Fish 补全' \
		'make install-fish         安装二进制和 Fish 补全' \
		'make install-bash         安装二进制和 Bash 补全' \
		'make install-zsh          安装二进制和 Zsh 补全' \
		'make install-completions  安装 Fish/Bash/Zsh 补全' \
		'make uninstall            卸载上述文件' \
		'make clean                清理构建产物'

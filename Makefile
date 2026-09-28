# Build and install xdg-desktop-portal-layercapture.
#
#   make                                # cargo build --release --locked
#   sudo make install PREFIX=/usr       # binary + portal, D-Bus and systemd files
#   sudo make uninstall PREFIX=/usr
#
# `install` never runs cargo, so `sudo make install` does not build as root: run `make` first.
# Packagers: DESTDIR is honoured; `install-data` installs only the data files (for build
# systems that install the binary themselves).

NAME := xdg-desktop-portal-layercapture

PREFIX ?= /usr/local
BINDIR ?= $(PREFIX)/bin
DATADIR ?= $(PREFIX)/share
SYSTEMDUSERUNITDIR ?= $(PREFIX)/lib/systemd/user
DBUSSERVICEDIR ?= $(DATADIR)/dbus-1/services
PORTALDIR ?= $(DATADIR)/xdg-desktop-portal/portals
DOCDIR ?= $(DATADIR)/doc/$(NAME)
LICENSEDIR ?= $(DATADIR)/licenses/$(NAME)

CARGO ?= cargo
CARGO_TARGET_DIR ?= target
BINARY ?= $(CARGO_TARGET_DIR)/release/$(NAME)

INSTALL ?= install
SED ?= sed

DBUS_SERVICE := org.freedesktop.impl.portal.desktop.layercapture.service
UNIT := $(NAME).service
PORTAL := layercapture.portal

.PHONY: all build check install install-bin install-data install-doc uninstall clean

all: build

build:
	$(CARGO) build --release --locked

check:
	$(CARGO) test --release --locked

install: install-bin install-data install-doc

install-bin:
	@test -x "$(BINARY)" || { echo "$(BINARY) not found; run 'make' first (as your user, not root)" >&2; exit 1; }
	$(INSTALL) -Dm755 "$(BINARY)" "$(DESTDIR)$(BINDIR)/$(NAME)"

# @bindir@ is replaced with the final BINDIR (without DESTDIR).
install-data:
	$(INSTALL) -Dm644 data/$(PORTAL) "$(DESTDIR)$(PORTALDIR)/$(PORTAL)"
	$(INSTALL) -d "$(DESTDIR)$(DBUSSERVICEDIR)" "$(DESTDIR)$(SYSTEMDUSERUNITDIR)"
	$(SED) 's|@bindir@|$(BINDIR)|g' data/$(DBUS_SERVICE).in > "$(DESTDIR)$(DBUSSERVICEDIR)/$(DBUS_SERVICE)"
	chmod 644 "$(DESTDIR)$(DBUSSERVICEDIR)/$(DBUS_SERVICE)"
	$(SED) 's|@bindir@|$(BINDIR)|g' data/$(UNIT).in > "$(DESTDIR)$(SYSTEMDUSERUNITDIR)/$(UNIT)"
	chmod 644 "$(DESTDIR)$(SYSTEMDUSERUNITDIR)/$(UNIT)"

install-doc:
	$(INSTALL) -Dm644 README.md "$(DESTDIR)$(DOCDIR)/README.md"
	$(INSTALL) -Dm644 NOTICE "$(DESTDIR)$(DOCDIR)/NOTICE"
	$(INSTALL) -Dm644 LICENSE "$(DESTDIR)$(LICENSEDIR)/LICENSE"

uninstall:
	rm -f "$(DESTDIR)$(BINDIR)/$(NAME)"
	rm -f "$(DESTDIR)$(PORTALDIR)/$(PORTAL)"
	rm -f "$(DESTDIR)$(DBUSSERVICEDIR)/$(DBUS_SERVICE)"
	rm -f "$(DESTDIR)$(SYSTEMDUSERUNITDIR)/$(UNIT)"
	rm -f "$(DESTDIR)$(DOCDIR)/README.md" "$(DESTDIR)$(DOCDIR)/NOTICE" "$(DESTDIR)$(LICENSEDIR)/LICENSE"
	-rmdir "$(DESTDIR)$(DOCDIR)" "$(DESTDIR)$(LICENSEDIR)" 2>/dev/null

clean:
	$(CARGO) clean

PREFIX ?= $(HOME)
CARGO  ?= cargo

.PHONY: build release test lint check run thumb install clean

build:
	cd crates && $(CARGO) build

release:
	cd crates && $(CARGO) build --release

test:
	cd crates && $(CARGO) test

lint:
	cd crates && $(CARGO) clippy --all-targets -- -D warnings

# What CI should run, and what to run before a commit.
check: test lint

MODEL ?= models/robot.vxm

run: build
	./crates/target/debug/voxeler $(MODEL)

# Render one frame to a PNG without opening a window -- the fastest way to see
# whether a rendering change did what you meant.
thumb: build
	./crates/target/debug/voxeler $(MODEL) --thumbnail /tmp/voxeler.png
	@echo "wrote /tmp/voxeler.png"

install: release
	install -d $(PREFIX)/bin
	install -m 755 crates/target/release/voxeler $(PREFIX)/bin/voxeler
	@echo "installed $(PREFIX)/bin/voxeler"

clean:
	cd crates && $(CARGO) clean

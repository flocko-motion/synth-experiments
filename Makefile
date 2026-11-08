# One Makefile for every generator; they all share the same CLI.
#
#   make build                      build everything
#   make list                       list generators
#   make demo psybass               endless at 148 bpm, plays through sox
#   make play percussion            stream into sox (endless unless BARS)
#   make render psybass             write a WAV (32 bars unless BARS)
#
# Options, for play/demo/render:  BPM=145 BARS=64 SEED=42 KEY=F# SCALE=minor
# The generator can also be given as GEN=psybass.

BPM   ?= 148
BARS  ?=
SEED  ?=
KEY   ?=
SCALE ?=

# Generators are the crates under generators/, named as in their Cargo.toml.
GENERATORS := $(shell sed -n 's/^name *= *"\(.*\)"/\1/p' generators/*/Cargo.toml)
BIN_DIR    := target/release

# `make play psybass`: take the name from the second goal and make that goal
# a no-op, so make doesn't try to build a target called "psybass".
RUN_GOALS := play demo render
ifneq ($(filter $(RUN_GOALS),$(firstword $(MAKECMDGOALS))),)
  GEN ?= $(word 2,$(MAKECMDGOALS))
  ifneq ($(word 2,$(MAKECMDGOALS)),)
    $(eval $(word 2,$(MAKECMDGOALS)):;@:)
  endif
endif

BIN  = $(BIN_DIR)/$(GEN)
OPTS = $(if $(SEED),--seed $(SEED)) $(if $(KEY),--key $(KEY)) $(if $(SCALE),--scale $(SCALE))
OUT ?= $(GEN)_$(BPM)bpm_$(or $(BARS),32)bars$(if $(SEED),_seed$(SEED)).wav

.PHONY: all build list play demo render clean check-gen

all: build

build:
	cargo build --release

list: build
	@for g in $(GENERATORS); do \
		printf '%-12s %s\n' "$$g" "$$($(BIN_DIR)/$$g --info | sed -n 2p)"; \
	done

check-gen:
	@if [ -z "$(GEN)" ]; then \
		echo "which generator? e.g. make $(firstword $(MAKECMDGOALS)) psybass"; \
		echo "available: $(GENERATORS)"; exit 2; \
	fi
	@case " $(GENERATORS) " in *" $(GEN) "*) ;; *) \
		echo "unknown generator '$(GEN)'; available: $(GENERATORS)"; exit 2;; \
	esac
	@cargo build --release -q -p $(GEN)

play: check-gen
	$(BIN) $(BPM) $(BARS) $(OPTS) | play -q --ignore-length -t wav -

demo: check-gen
	$(BIN) --demo $(OPTS)

render: check-gen
	$(BIN) $(BPM) $(or $(BARS),32) $(OPTS) --out $(OUT)

clean:
	cargo clean
	rm -f *.wav

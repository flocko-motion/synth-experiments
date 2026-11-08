# One Makefile for every generator; they all share the same CLI.
#
#   make build                      build everything, generators and workspace
#   make list                       list generators
#   make demo psybass               endless at 148 bpm, plays through sox
#   make play percussion            stream into sox (endless unless BARS)
#   make render psybass             write a WAV (32 bars unless BARS)
#
# Options, for play/demo/render:  BPM=145 BARS=64 SEED=42 KEY=F# SCALE=minor
# The generator can also be given as GEN=psybass.
#
# Effects (effects/) work the same way, fed from a source generator:
#   make demo duck                  acid through duck
#   make play duck SOURCE=psybass   pick the source
#
# The workspace mixer (workspace/, binary `ws`), see docs/workspace.md:
#
#   make ws                         build just the workspace
#   make daemon                     run the workspace daemon (BPM, KEY, SCALE)
#   make add psybass                add a generator to the running daemon
#                                   (SEED; NAME=bass to pick its name, else
#                                   psybass, psybass-2, …)
#   make rm psybass                 remove it again

BPM   ?= 148
BARS  ?=
SEED  ?=
KEY   ?=
SCALE ?=

# Generators are the crates under generators/, named as in their Cargo.toml.
# Effects are the crates under effects/; they take audio on stdin when run
# standalone, so play/demo/render pipe SOURCE into them.
EFFECTS    := $(shell sed -n 's/^name *= *"\(.*\)"/\1/p' effects/*/Cargo.toml)
GENERATORS := $(shell sed -n 's/^name *= *"\(.*\)"/\1/p' generators/*/Cargo.toml) $(EFFECTS)
SOURCE     ?= acid
BIN_DIR    := target/release

# `make play psybass`: take the name from the second goal and make that goal
# a no-op, so make doesn't try to build a target called "psybass".
RUN_GOALS := play demo render add rm
ifneq ($(filter $(RUN_GOALS),$(firstword $(MAKECMDGOALS))),)
  GEN ?= $(word 2,$(MAKECMDGOALS))
  ifneq ($(word 2,$(MAKECMDGOALS)),)
    $(eval $(word 2,$(MAKECMDGOALS)):;@:)
  endif
endif

BIN  = $(BIN_DIR)/$(GEN)
# For an effect: the source command and a pipe, else nothing.
FEED = $(if $(filter $(GEN),$(EFFECTS)),$(BIN_DIR)/$(SOURCE) $(1) |)
OPTS = $(if $(SEED),--seed $(SEED)) $(if $(KEY),--key $(KEY)) $(if $(SCALE),--scale $(SCALE))
OUT ?= $(GEN)_$(BPM)bpm_$(or $(BARS),32)bars$(if $(SEED),_seed$(SEED)).wav

WS   = $(BIN_DIR)/ws
NAME ?= $(GEN)

.PHONY: all build list play demo render clean check-gen ws daemon add rm

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
	@cargo build --release -q -p $(GEN) $(if $(filter $(GEN),$(EFFECTS)),-p $(SOURCE))

play: check-gen
	$(call FEED,$(BPM) $(BARS) $(OPTS)) $(BIN) $(BPM) $(BARS) $(OPTS) | play -q --ignore-length -t wav -

demo: check-gen
	$(call FEED,$(OPTS)) $(BIN) --demo $(OPTS)

render: check-gen
	$(call FEED,$(BPM) $(or $(BARS),32) $(OPTS)) $(BIN) $(BPM) $(or $(BARS),32) $(OPTS) --out $(OUT)

ws:
	cargo build --release -p workspace

# Takes BPM only when given on the command line, so the daemon's own
# default applies otherwise.
daemon: ws
	$(WS) daemon $(if $(filter command line,$(origin BPM)),--bpm $(BPM)) \
		$(if $(KEY),--key $(KEY)) $(if $(SCALE),--scale $(SCALE))

add: check-gen ws
	$(WS) add $(if $(filter command line,$(origin NAME)),--name $(NAME)) $(BIN) $(if $(SEED),--seed $(SEED))

rm: ws
	@if [ -z "$(NAME)" ]; then echo "which one? e.g. make rm psybass"; exit 2; fi
	$(WS) rm $(NAME)

clean:
	cargo clean
	rm -f *.wav

# GalOS Benchmark - Cross-compile with musl
#
# Usage:
#   make                    # default: riscv64
#   make ARCH=aarch64
#   make ARCH=loongarch64
#   make install            # copy into rootfs disk image

ARCH ?= riscv64

ifeq ($(ARCH),x86_64)
  CROSS_COMPILE ?= x86_64-linux-musl-
else ifeq ($(ARCH),aarch64)
  CROSS_COMPILE ?= aarch64-linux-musl-
else ifeq ($(ARCH),loongarch64)
  CROSS_COMPILE ?= loongarch64-linux-musl-
else
  CROSS_COMPILE ?= riscv64-linux-musl-
endif

CC      = $(CROSS_COMPILE)gcc
CFLAGS  = -static -O2 -Wall -Wextra
TARGET  = bench
DISK    = ../../make/disk.img

all: $(TARGET)

$(TARGET): bench.c
	$(CC) $(CFLAGS) -o $@ $<
	@echo "Built: $(TARGET) ($(ARCH))"

install: $(TARGET)
	@if [ ! -f $(DISK) ]; then echo "Error: $(DISK) not found. Run 'make rootfs' first."; exit 1; fi
	@which e2cp > /dev/null 2>&1 || { echo "Error: e2cp not found. Install e2tools."; exit 1; }
	e2cp $(TARGET) $(DISK):/usr/bin/bench
	@echo "Installed bench into $(DISK):/usr/bin/bench"

clean:
	rm -f $(TARGET)

.PHONY: all clean install

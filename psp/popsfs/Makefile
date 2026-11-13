# Build with the pspdev toolchain, for example:
#   docker run --rm -v "$PWD":/src -w /src pspdev/pspdev make
TARGET = popsfs
OBJS = main.o imports.o

CFLAGS = -std=c99 -Os -G0 -Wall -Werror -fno-pic -fno-builtin
ASFLAGS = $(CFLAGS)

BUILD_PRX = 1
PRX_EXPORTS = exports.exp

USE_KERNEL_LIBC = 1
USE_KERNEL_LIBS = 1
LDFLAGS = -nostartfiles
LIBS =

PSPSDK = $(shell psp-config --pspsdk-path)
include $(PSPSDK)/lib/build.mak

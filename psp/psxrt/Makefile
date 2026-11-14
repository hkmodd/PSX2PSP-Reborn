# Build with the pspdev toolchain (WSL: /usr/local/pspdev), then copy the module the converter
# embeds:  make && cp psxrt.prx ../../resources/psxrt.prx
TARGET = psxrt
OBJS = main.o

CFLAGS = -std=gnu99 -O2 -G0 -Wall -Wextra -Werror
ASFLAGS = $(CFLAGS)

BUILD_PRX = 1

PSPSDK = $(shell psp-config --pspsdk-path)
include $(PSPSDK)/lib/build.mak

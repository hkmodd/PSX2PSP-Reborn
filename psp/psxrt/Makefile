# Build with the pspdev toolchain (WSL: /usr/local/pspdev), then copy the module the converter
# embeds:  make && cp psxrt.prx ../../resources/psxrt.prx
TARGET = psxrt
CORE = bios card cdrom cpu dma gpu gte mdec mem psx sio spu timers
OBJS = main.o disc.o ge.o native.o snap.o trampolines.o $(addprefix core/,$(addsuffix .o,$(CORE)))

INCDIR = core
CFLAGS = -std=gnu99 -O2 -G0 -Wall -Wextra -Werror
ASFLAGS = $(CFLAGS)
LIBS = -lpspgu -lpspaudio -lpsppower -lz

BUILD_PRX = 1

PSPSDK = $(shell psp-config --pspsdk-path)
include $(PSPSDK)/lib/build.mak

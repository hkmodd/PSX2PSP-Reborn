# Build with the pspdev toolchain (WSL: /usr/local/pspdev), then copy the module the converter
# embeds:  make && cp psxrt.prx ../../resources/psxrt.prx
TARGET = psxrt
CORE = bios card cdrom cpu dma gpu gte mdec mem pollscan psx sio spu timers
OBJS = main.o disc.o ge.o gte_fast.o kubridge.o native.o sas.o snap.o trampolines.o vlc.o $(addprefix core/,$(addsuffix .o,$(CORE)))

# gte_fast.c: v1, a1-a3 and t0-t9 callee-saved, so psxrt_gte_cmd need not save them. No sibling
# calls: their epilogue would restore a1-a3 over the arguments just set (the caller's a1 as cmd).
gte_fast.o: CFLAGS += -fno-optimize-sibling-calls $(foreach r,3 5 6 7 8 9 10 11 12 13 14 15 24 25,'-fcall-saved-$$$(r)')

INCDIR = core
CFLAGS = -std=gnu99 -O2 -G0 -Wall -Wextra -Werror
ASFLAGS = $(CFLAGS)
LIBS = -lpspgu -lpspaudio -lpsppower -lpspsascore -lz

BUILD_PRX = 1

PSPSDK = $(shell psp-config --pspsdk-path)
include $(PSPSDK)/lib/build.mak

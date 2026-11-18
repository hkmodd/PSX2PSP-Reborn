# me_probe: probe 0 of the ME translator, an EBOOT for a PSP with ARK-4 (main.c). With the pspdev
# toolchain (WSL: /usr/local/pspdev):  make  ->  EBOOT.PBP; beside it on the PSP goes sheep.gedp,
# a dump of the sim (psp/psxrt/ge_dump.h).
#
# The translator (ge.c, gpu.c, ge_host_pc.c, gel_hash.c, replay.c) is linked once into tr/all.o,
# its statics on whole 64-byte lines (pad_*.S), then copied seven times, each with its own
# statics: copyN.o has only trN_run global, and its calloc, free and abort are trN_calloc, trN_free
# and cpu_abort (copies 0-4, the CPU's) or me_abort (5, 6, the ME's) (me.c). The C library calls
# with state of their own (the sim's dump and log paths: no replay reaches them) are me_trap_<call>
# on the ME's copies, so that a call shows in the log instead of corrupting the CPU's.
ME_TRAPS = fopen fwrite fclose getenv snprintf strtoul
TARGET = me_probe
OBJS = main.o me.o $(foreach n,0 1 2 3 4 5 6,copy$(n).o)

PSXRT = ../psxrt
INCDIR = $(PSXRT) $(PSXRT)/core
CFLAGS = -std=gnu99 -O2 -G0 -Wall -Wextra -Werror -fno-common -DGEH_PC -DGEL_ADDR=geh_addr
ASFLAGS = $(CFLAGS)
LIBS = -lpspkubridge -lpspgu -lpsppower

EXTRA_TARGETS = EBOOT.PBP
PSP_EBOOT_TITLE = ME probe 0

PSPSDK = $(shell psp-config --pspsdk-path)
include $(PSPSDK)/lib/build.mak

# the ME's code: no loop turned into a call it need not make
me.o: CFLAGS += -fno-tree-loop-distribute-patterns

TR_OBJS = tr/pad_head.o tr/ge.o tr/gpu.o tr/ge_host_pc.o tr/gel_hash.o tr/replay.o tr/pad_tail.o
tr:
	mkdir -p tr
tr/%.o: $(PSXRT)/%.c | tr
	$(CC) $(CFLAGS) -c $< -o $@
tr/%.o: $(PSXRT)/core/%.c | tr
	$(CC) $(CFLAGS) -c $< -o $@
tr/%.o: %.c | tr
	$(CC) $(CFLAGS) -c $< -o $@
tr/%.o: %.S | tr
	$(CC) $(ASFLAGS) -c $< -o $@
tr/all.o: $(TR_OBJS)
	psp-ld -r $(TR_OBJS) -o $@
copy%.o: tr/all.o Makefile
	psp-objcopy -G replay_run $< tr/local$*.o
	psp-objcopy --redefine-sym replay_run=tr$*_run --redefine-sym calloc=tr$*_calloc --redefine-sym free=tr$*_free \
		--redefine-sym abort=$(if $(filter 5 6,$*),me_abort,cpu_abort) --redefine-sym psx_log=pr_log \
		--redefine-sym psx_raise=pr_raise --redefine-sym psx_video_mode=pr_video 		$(if $(filter 5 6,$*),$(foreach f,$(ME_TRAPS),--redefine-sym $(f)=me_trap_$(f))) tr/local$*.o $@

clean: clean-tr
clean-tr:
	rm -rf tr

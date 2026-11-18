/*
 * psxrt: what ge.c asks of the machine it runs on, step 1b of the pure translator (me-translator
 * design note): the clock, the data cache, memory, the uncached mirror, VRAM and the GE's queue.
 * ge.c calls nothing else, so the same translator builds its lists on the PSP, on the ME or in the
 * sim, each with its own host. The PSP's host is here, inline: the same calls ge.c made itself.
 * On psxrt's Media Engine (me.c walks the GPU's lists there, ME translator 2b-2) the calls that are
 * system calls go to me.c instead: the clock the SC publishes, the ME's own cache, list calls the
 * SC serves and the stall as a word the SC applies. geh_me() tells the ME by its stack, in the ME's
 * eDRAM at 0x80000000-0x801FFFFF; ge.c on the SC runs on a user stack, below 0x80000000.
 * Another host defines the same names (the declarations below are the contract). GEH_PC takes
 * the contract on the PSP too: me_probe runs ge.c there with ge_host_pc.c, on the CPU and the ME.
 */
#ifndef PSXRT_GE_HOST_H
#define PSXRT_GE_HOST_H

#include "gel.h"
#include "psx.h"

#if defined(__psp__) && !defined(GEH_PC)

#include <pspge.h>
#include <pspgu.h>
#include <pspkernel.h>
#include <pspsysmem.h>
#include <psputils.h>

/* 1 on the Media Engine */
static inline int geh_me(void)
{
    u32 sp;
    __asm__("move %0, $sp" : "=r"(sp));
    return (int)(sp >> 31);
}
/* me.c, on the ME: the SC's clock, the ME's whole data cache written back and dropped, and the list
 * calls below */
u32 me_geh_us(void);
void me_geh_wbinv_all(void);
int me_geh_list_start(void *buf);
void me_geh_list_stall(int id, void *buf, u32 n);
u32 me_geh_list_finish(void);
void me_geh_list_wait(void);

/* microseconds, wrapping */
static inline u32 geh_us(void) { return geh_me() ? me_geh_us() : sceKernelGetSystemTimeLow(); }
/* the data cache's lines over [p, p+n) written back; and dropped (on the ME: all of them) */
static inline void geh_wb(const void *p, u32 n)
{
    if (geh_me())
        me_geh_wbinv_all();
    else
        sceKernelDcacheWritebackRange(p, n);
}
static inline void geh_wbinv(const void *p, u32 n)
{
    if (geh_me())
        me_geh_wbinv_all();
    else
        sceKernelDcacheWritebackInvalidateRange(p, n);
}
/* p through the uncached mirror */
#define GEH_UNCACHED(p) ((void *)((u32)(p) | 0x40000000u))
/* PSP VRAM, as the CPU and the GE's block transfer address it */
#define GEH_VRAM ((void *)0x04000000u)

/* bytes of user memory, 64-byte aligned; NULL when there are none */
static inline void *geh_alloc(const char *name, u32 bytes)
{
    SceUID uid = sceKernelAllocPartitionMemory(2, name, PSP_SMEM_Low, (SceSize)(bytes + 64), NULL);
    return uid < 0 ? NULL : (void *)(((u32)sceKernelGetBlockHeadAddr(uid) + 63) & ~63u);
}
/* the largest block geh_alloc could still have */
static inline u32 geh_free(void) { return (u32)sceKernelMaxFreeMemSize(); }
/* memory of ge.c's own that its lists address (the GE reads the PSP's directly) */
static inline void geh_region(const void *p, u32 bytes) { (void)p, (void)bytes; }

/* The list: pspgu's GU_SEND context, which main.c's present writes too. Its head and the scissor
 * pspgu keeps for it (GuContext: 92 bytes, scissor_enable at 12), so that sceGuEnable
 * (GU_SCISSOR_TEST) there sends the last scissor either side set. */
extern char gu_contexts[];
extern GelList *gu_list; /* GuListHead begins as GelList */
#define GEH_LIST gu_list
#define GEH_SCISSOR ((GelScissor *)(gu_contexts + GU_SEND * 92 + 12))

/* Opens the list at buf and queues it with its stall at the start; the queue's id. */
static inline int geh_list_start(void *buf)
{
    if (geh_me())
        return me_geh_list_start(buf);
    sceGuStart(GU_SEND, buf);
    return sceGeListEnQueue(GEH_UNCACHED(buf), GEH_UNCACHED(buf), -1, NULL);
}
/* The GE may run the list's first n bytes. */
static inline void geh_list_stall(int id, void *buf, u32 n)
{
    if (geh_me())
        me_geh_list_stall(id, buf, n);
    else
        sceGeListUpdateStallAddr(id, (u8 *)GEH_UNCACHED(buf) + n);
}
/* Ends the list (FINISH, END); its length in bytes. */
static inline u32 geh_list_finish(void) { return geh_me() ? me_geh_list_finish() : (u32)sceGuFinish(); }
/* Waits for the GE to finish the list. */
static inline void geh_list_wait(void)
{
    if (geh_me())
        me_geh_list_wait();
    else
        sceGuSync(0, 0);
}

/* At boot: gel.h against pspgu over an idle list (gel_check.c); the line to log, 0 for none. */
int gel_check(unsigned int *buf, unsigned int words, char *out, int cap);
static inline int geh_check(void *buf, u32 words, char *out, int cap) { return gel_check(buf, words, out, cap); }

#else

u32 geh_us(void);
void geh_wb(const void *p, u32 n);
void geh_wbinv(const void *p, u32 n);
#define GEH_UNCACHED(p) ((void *)(p))
extern u16 *geh_vram;
#define GEH_VRAM ((void *)geh_vram)
void *geh_alloc(const char *name, u32 bytes);
u32 geh_free(void);
void geh_region(const void *p, u32 bytes);
extern GelList *geh_list;
extern GelScissor geh_scissor;
#define GEH_LIST geh_list
#define GEH_SCISSOR (&geh_scissor)
int geh_list_start(void *buf);
void geh_list_stall(int id, void *buf, u32 n);
u32 geh_list_finish(void);
void geh_list_wait(void);
int geh_check(void *buf, u32 words, char *out, int cap);

#endif

#endif

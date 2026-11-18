/*
 * me_probe: a GE translator dump run again on a PC (replay.h). Each frame's list hash, as the
 * sim logs it, and whether it is the sim's. Exit 0 when the dump read through, and, in
 * GE_SHADOW mode, every frame and every call's return were the sim's.
 *
 *   replay_pc <dump> [shadow|normal] [-v] [-nohash] [-decode]
 *
 * -nohash: as me_probe's timing runs, no list or memory hashed (no frame is then the sim's).
 * -decode: the dump read, ge.c never called (me_probe's C0d, M0d).
 */
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "ge.h"
#include "replay.h"

static int verbose;

/* the core's calls out that gpu.c and ge.c make */
void psx_log(Psx *psx, const char *fmt, ...)
{
    (void)psx;
    if (!verbose)
        return;
    va_list ap;
    va_start(ap, fmt);
    vfprintf(stderr, fmt, ap);
    va_end(ap);
    fputc('\n', stderr);
}
void psx_raise(Psx *psx, int irq) { (void)psx, (void)irq; }
void psx_video_mode(Psx *psx) { (void)psx; }

static void on_frame(void *user, const ReplayFrame *f)
{
    const GehFrame *o = &f->got;
    int mode = *(const int *)user;
    printf("geh: frame %u, %u lists, %u words, %u prims, %u calls, %u misses, %u unknown: %08X%08X  %u polys%s\n",
           (unsigned)o->frame, (unsigned)o->lists, (unsigned)o->words, (unsigned)o->prims, (unsigned)o->calls,
           (unsigned)o->misses, (unsigned)o->unknown, (unsigned)(o->h >> 32), (unsigned)o->h, (unsigned)f->polys,
           mode != GE_SHADOW ? "" : f->match ? "  = sim" : "  DIFF");
    if (o->mem)
        printf("geh: frame %u memory %08X%08X%s\n", (unsigned)o->frame, (unsigned)(o->mh >> 32), (unsigned)o->mh,
               mode != GE_SHADOW ? "" : f->match ? "  = sim" : "  DIFF");
    if (mode == GE_SHADOW && !f->match)
        printf("  sim: %u lists, %u words, %u prims, %u calls, %u misses, %u unknown: %08X%08X\n", (unsigned)f->want[1],
               (unsigned)f->want[2], (unsigned)f->want[3], (unsigned)f->want[4], (unsigned)f->want[5], (unsigned)f->want[6],
               (unsigned)f->want[7], (unsigned)f->want[8]);
}

int main(int argc, char **argv)
{
    int mode = GE_SHADOW, no_hash = 0, decode = 0;
    const char *path = NULL;
    for (int i = 1; i < argc; i++)
        if (!strcmp(argv[i], "shadow"))
            mode = GE_SHADOW;
        else if (!strcmp(argv[i], "normal"))
            mode = GE_NORMAL;
        else if (!strcmp(argv[i], "-v"))
            verbose = 1;
        else if (!strcmp(argv[i], "-nohash"))
            no_hash = 1;
        else if (!strcmp(argv[i], "-decode"))
            decode = 1;
        else
            path = argv[i];
    if (!path) {
        fprintf(stderr, "replay_pc <dump> [shadow|normal] [-v] [-nohash] [-decode]\n");
        return 2;
    }
    FILE *fp = fopen(path, "rb");
    if (!fp)
        return perror(path), 2;
    fseek(fp, 0, SEEK_END);
    long bytes = ftell(fp);
    fseek(fp, 0, SEEK_SET);
    u32 *d = (u32 *)malloc((size_t)bytes + 4);
    if (!d || bytes < 0 || fread(d, 1, (size_t)bytes, fp) != (size_t)bytes)
        return fprintf(stderr, "%s: cannot read\n", path), 2;
    fclose(fp);
    static PsxHost host;
    Psx *psx = (Psx *)calloc(1, sizeof *psx);
    psx->host = &host;
    psx->gpu.vram = (u16 *)calloc(VRAM_W * VRAM_H, 2);
    psx->ram = (u8 *)calloc(2 * 1024 * 1024, 1);
    ReplayHooks h = {NULL, on_frame, &mode, no_hash, decode};
    ReplayResult r;
    int ok = replay_run(psx, d, (u32)(bytes / 4), mode, &h, &r);
    printf("replay %s: %u events, %u frames (%u not the sim's), %u returns not the sim's, %u polys after the first frame%s%s\n",
           mode == GE_SHADOW ? "shadow" : "normal", (unsigned)r.events, (unsigned)r.frames, (unsigned)r.frames_bad,
           (unsigned)r.rets_bad, (unsigned)r.polys, ok ? "" : "; ERROR: ", ok ? "" : r.err);
    return ok && !r.frames_bad && !r.rets_bad ? 0 : 1;
}

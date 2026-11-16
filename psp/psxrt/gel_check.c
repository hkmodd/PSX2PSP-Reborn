/*
 * psxrt: gel.h against pspgu, at boot. The same calls go once through pspgu, in its GU_CALL
 * context (which nothing else uses), and once through gel.h, into the same words of an idle list;
 * every word and the scissor pspgu keeps must come out equal. Covers the arguments ge.c passes
 * and the edges around them: addresses in RAM, VRAM and the uncached mirrors, sizes, all states.
 */
#include <pspgu.h>
#include <stdio.h>
#include <string.h>

#include "gel.h"

int gel_check(unsigned int *buf, unsigned int words, char *out, int cap);

extern char gu_contexts[];

static void *const ptrs[] = {(void *)0x08800010, (void *)0x48812340, (void *)0x04000000, (void *)0x04088000,
                             (void *)0x09FFFFF0, (void *)0x44154000, (void *)0x0A123450, (void *)0};
#define NP (int)(sizeof ptrs / sizeof ptrs[0])

static const int dith[2][16] = {{-4, 0, -3, 1, 2, -2, 3, -1, -3, 1, -4, 0, 3, -1, 2, -2},
                                {-8, 7, -1, 0, 15, 16, -16, 5, 3, 3, 3, 3, -7, 6, -5, 4}};

static const int sizes[] = {1, 2, 3, 4, 8, 16, 64, 128, 255, 256, 257, 512, 1000};
#define NS (int)(sizeof sizes / sizeof sizes[0])

/* g: through pspgu (its current list), else through gel.h into l with the scissor sc */
static void battery(int g, GelList *l, GelScissor *sc)
{
#define K(fn, gfn, ...)                                                                                                    \
    do {                                                                                                                   \
        if (g)                                                                                                             \
            fn(__VA_ARGS__);                                                                                               \
        else                                                                                                               \
            gfn(l, __VA_ARGS__);                                                                                           \
    } while (0)
#define SC(...)                                                                                                            \
    do {                                                                                                                   \
        if (g)                                                                                                             \
            sceGuScissor(__VA_ARGS__);                                                                                     \
        else                                                                                                               \
            gel_scissor(l, sc, __VA_ARGS__);                                                                               \
    } while (0)
    /* the scissor is stored while off, sent by the enable, then with each change */
    SC(5, 6, 7, 8);
    if (g)
        sceGuEnable(GU_SCISSOR_TEST);
    else
        gel_scissor_on(l, sc);
    SC(0, 0, 1024, 512);
    SC(0, 0, 480, 272);
    SC(17, 3, 1, 1);
    SC(1023, 511, 1, 1);
    SC(100, 200, 300, 100);
    for (int s = 0; s <= 20; s++)
        if (s != GU_SCISSOR_TEST) {
            K(sceGuEnable, gel_enable, s);
            K(sceGuDisable, gel_disable, s);
        }
    for (int i = 0; i < NP; i++) {
        K(sceGuDrawBufferList, gel_draw_buffer_list, i & 1 ? GU_PSM_5650 : GU_PSM_5551, ptrs[i], i & 1 ? 512 : 1024);
        K(sceGuClutLoad, gel_clut_load, i & 1 ? 32 : 2, ptrs[i]);
        K(sceGuClutLoad, gel_clut_load, 64, ptrs[i]);
        for (int k = 0; k < 4; k++)
            K(sceGuCopyImage, gel_copy_image, k, i, 2 * i, 1024 >> k, 512 >> k, 1024, ptrs[i], 3 * i, i, 512, ptrs[(i + 3) % NP]);
    }
    static const int cmds[][2] = {{0x15, 0}, {0x16, 511 << 10 | 1023}, {0xC9, 0x10100}, {0xC9, 0x101}, {0x12, 0x80011F},
                                  {0x0B, 0},  {0x10, -1},                {0x0A, 0x123456}, {0xFF, 0x7FFFFFFF}};
    for (int i = 0; i < (int)(sizeof cmds / sizeof cmds[0]); i++)
        K(sceGuSendCommandi, gel_cmd, cmds[i][0], cmds[i][1]);
    K(sceGuOffset, gel_offset, 2048 - 512, 2048 - 256);
    K(sceGuOffset, gel_offset, 2048 - 240, 2048 - 136);
    K(sceGuViewport, gel_viewport, 2048, 2048, 1024, 512);
    K(sceGuViewport, gel_viewport, 2048, 2048, 480, 272);
    K(sceGuViewport, gel_viewport, 0, 1, 3, 1);
    K(sceGuViewport, gel_viewport, 100, -100, 1023, 511);
    K(sceGuDepthMask, gel_depth_mask, GU_TRUE);
    K(sceGuDepthMask, gel_depth_mask, GU_FALSE);
    static const unsigned int masks[] = {0, 0xFFFFFFFF, 0xFFFF00FF, 0xFF00FFFF, 0x12345678};
    for (int i = 0; i < 5; i++)
        K(sceGuPixelMask, gel_pixel_mask, masks[i]);
    K(sceGuShadeModel, gel_shade_model, GU_SMOOTH);
    K(sceGuShadeModel, gel_shade_model, GU_FLAT);
    for (int i = 0; i < 2; i++) {
        if (g)
            sceGuSetDither((const ScePspIMatrix4 *)dith[i]);
        else
            gel_set_dither(l, dith[i]);
    }
    for (int u = 0; u < 2; u++)
        for (int v = 0; v < 2; v++)
            K(sceGuTexWrap, gel_tex_wrap, u, v);
    for (int a = 0; a < 8; a++)
        K(sceGuTexFilter, gel_tex_filter, a, 7 - a);
    for (int f = 0; f < 5; f++)
        for (int c = 0; c < 2; c++)
            K(sceGuTexFunc, gel_tex_func, f, c);
    for (int p = 0; p <= 10; p++)
        K(sceGuTexMode, gel_tex_mode, p, p & 7, p & 1, p >> 1 & 1);
    for (int i = 0; i < NP; i++)
        for (int w = 0; w < NS; w++)
            for (int h = 0; h < NS; h += 3)
                K(sceGuTexImage, gel_tex_image, (w + h) % 3 == 2 ? 7 : (w + h) % 3, sizes[w], sizes[h], i & 1 ? 1280 : sizes[w], ptrs[i]);
    if (g)
        sceGuTexFlush();
    else
        gel_tex_flush(l);
    static const int refs[] = {0, 0x80, 0xFF, 0x1FF};
    for (int f = 0; f < 8; f++)
        for (int r = 0; r < 4; r++) {
            K(sceGuStencilFunc, gel_stencil_func, f, refs[r], r & 1 ? 0 : 0xFF);
            K(sceGuAlphaFunc, gel_alpha_func, f, refs[r], r & 1 ? 0xFF : 0x0F);
        }
    for (int a = 0; a < 6; a++)
        for (int b = 0; b < 6; b++)
            for (int c = 0; c < 6; c++)
                K(sceGuStencilOp, gel_stencil_op, a, b, c);
    static const unsigned int fix[][2] = {{0x7D7D7D, 0x7D7D7D}, {0xFFFFFF, 0x3A3A3A}, {0x12345678, 0xFFFFFFFF}};
    for (int op = 0; op < 6; op++)
        for (int s = 0; s <= 10; s++)
            for (int d = 0; d <= 10; d++)
                K(sceGuBlendFunc, gel_blend_func, op, s, d, fix[(s + d) % 3][0], fix[(s + d) % 3][1]);
    for (int c = 0; c < 4; c++)
        for (int s = 0; s < 32; s += 8)
            K(sceGuClutMode, gel_clut_mode, c, s, s ? 0xFF : 0x0F, c & 1);
    static const int vt[] = {0, GU_COLOR_8888 | GU_VERTEX_32BITF | GU_TRANSFORM_2D,
                             GU_TEXTURE_32BITF | GU_COLOR_8888 | GU_VERTEX_32BITF | GU_TRANSFORM_2D,
                             GU_TEXTURE_16BIT | GU_VERTEX_16BIT | GU_TRANSFORM_2D};
    static const int counts[] = {1, 2, 3, 65535};
    for (int size = 0; size <= 40; size++) {
        void *m = g ? sceGuGetMemory(size) : gel_get_memory(l, size);
        K(sceGuDrawArray, gel_draw_array, size % 7, vt[size & 3], counts[size >> 2 & 3], size & 4 ? m : NULL, size & 8 ? NULL : m);
    }
    void *big = g ? sceGuGetMemory(16 * 256 + 16) : gel_get_memory(l, 16 * 256 + 16);
    for (int p = 0; p < 7; p++)
        for (int i = 0; i < NP; i++)
            K(sceGuDrawArray, gel_draw_array, p, vt[i & 3], counts[(p + i) & 3], i & 2 ? ptrs[i] : NULL, i & 4 ? big : ptrs[i]);
#undef K
#undef SC
}

/* buf: an idle list of `words` (cached address; both sides write it uncached). */
int gel_check(unsigned int *buf, unsigned int words, char *out, int cap)
{
    unsigned int *ub = (unsigned int *)((unsigned int)buf | 0x40000000u), half = words / 2, *ref = ub + half;
    GelScissor *ctx = (GelScissor *)(gu_contexts + GU_CALL * 92 + 12), sc0 = *ctx, sc;
    memset(ub, 0xA5, half * 4);
    sceGuStart(GU_CALL, buf);
    battery(1, NULL, NULL);
    unsigned int n_gu = (unsigned int)sceGuCheckList() / 4;
    sceGuFinish();
    GelScissor sc_gu = *ctx;
    *ctx = sc0;
    if (n_gu > half)
        return snprintf(out, (size_t)cap, "gel check: %u words overran %u", n_gu, half);
    memcpy(ref, ub, n_gu * 4);
    memset(ub, 0xA5, half * 4);
    sc = sc0;
    GelList l = {ub, ub};
    battery(0, &l, &sc);
    unsigned int n_gel = (unsigned int)(l.current - ub), differ = 0, first = ~0u;
    for (unsigned int i = 0; i < n_gu && i < n_gel; i++)
        if (ub[i] != ref[i] && !differ++)
            first = i;
    int sc_ok = !memcmp(&sc, &sc_gu, sizeof sc);
    int n = snprintf(out, (size_t)cap, "gel check: %u words through pspgu, %u through gel.h, %u differ", n_gu, n_gel, differ);
    if (differ && n < cap)
        n += snprintf(out + n, (size_t)(cap - n), " (first at %u: pspgu %08X, gel %08X)", first, ref[first], ub[first]);
    if (n < cap)
        n += snprintf(out + n, (size_t)(cap - n), "; scissor %s", sc_ok ? "equal" : "DIFFERS");
    return n;
}

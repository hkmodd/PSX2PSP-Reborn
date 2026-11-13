#include "hud.h"

/* PS1 CPU clock: 33.8688 MHz, as cycles per 10^4 microseconds. */
#define PS1_CYCLES_PER_10K_US 338688ull

void hud_compute(HudStats *s, unsigned int us, unsigned int flips, unsigned int cycles,
                 unsigned int idle_us)
{
    unsigned long long den;

    s->fps = s->emu = s->cpu = HUD_NONE;
    if (us == 0)
        return;
    s->fps = (unsigned int)(((unsigned long long)flips * 1000000 + us / 2) / us);
    /* cycles * 100 / (us * 33.8688) */
    den = (unsigned long long)us * PS1_CYCLES_PER_10K_US;
    s->emu = (unsigned int)(((unsigned long long)cycles * 1000000 + den / 2) / den);
    if (s->emu > 999) /* the counter jumped: POPS reset or rebased it */
        s->emu = HUD_NONE;
    /* More idle time than real time means the clock is not in microseconds: show nothing. */
    if ((unsigned long long)idle_us * 20 <= (unsigned long long)us * 21)
        s->cpu = idle_us >= us ? 0
                               : 100 - (unsigned int)(((unsigned long long)idle_us * 100 + us / 2) / us);
}

static char *put_num(char *p, unsigned int v, int width)
{
    char digits[10];
    int n = 0;

    if (v == HUD_NONE) {
        while (width--)
            *p++ = '-';
        return p;
    }
    do {
        digits[n++] = (char)('0' + v % 10);
        v /= 10;
    } while (v && n < 10);
    while (width-- > n)
        *p++ = ' ';
    while (n)
        *p++ = digits[--n];
    return p;
}

static char *put_str(char *p, const char *s)
{
    while (*s)
        *p++ = *s++;
    return p;
}

int hud_format(char *out, const HudStats *s)
{
    char *p = out;

    p = put_str(p, "FPS ");
    p = put_num(p, s->fps, 2);
    p = put_str(p, " EMU ");
    p = put_num(p, s->emu, 3);
    p = put_str(p, "% CPU ");
    p = put_num(p, s->cpu, 3);
    p = put_str(p, "%");
    *p = 0;
    return (int)(p - out);
}

/* 5x7 glyphs, one byte per row, bit 4 = leftmost column. */
static const char glyph_chars[] = "0123456789CEFMPSU%-";
static const unsigned char glyphs[][7] = {
    {0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E}, /* 0 */
    {0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E}, /* 1 */
    {0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F}, /* 2 */
    {0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E}, /* 3 */
    {0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02}, /* 4 */
    {0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E}, /* 5 */
    {0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E}, /* 6 */
    {0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08}, /* 7 */
    {0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E}, /* 8 */
    {0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C}, /* 9 */
    {0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E}, /* C */
    {0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F}, /* E */
    {0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10}, /* F */
    {0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11}, /* M */
    {0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10}, /* P */
    {0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E}, /* S */
    {0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E}, /* U */
    {0x18, 0x19, 0x02, 0x04, 0x08, 0x13, 0x03}, /* % */
    {0x00, 0x00, 0x00, 0x1F, 0x00, 0x00, 0x00}, /* - */
};

#define SCALE 2
#define CELL_W (6 * SCALE) /* 5 columns and a gap */
#define CELL_H (7 * SCALE)
#define PAD 3

int hud_width(int len)
{
    return len * CELL_W - SCALE + 2 * PAD;
}

int hud_height(void)
{
    return CELL_H + 2 * PAD;
}

static const unsigned char *glyph(char c)
{
    int i;

    for (i = 0; glyph_chars[i]; i++)
        if (glyph_chars[i] == c)
            return glyphs[i];
    return 0;
}

/*
 * The box is drawn on every frame, so the glyphs are looked up once and each row is built
 * in `on` before it is written: the frame buffer only sees one store per pixel.
 */
void hud_draw(void *fb, int stride, int format, int x, int y, const char *text)
{
    const unsigned char *g[HUD_TEXT_MAX];
    unsigned char on[HUD_TEXT_MAX * CELL_W + 2 * PAD];
    int len = 0, w, h = hud_height(), px, py, i, c;

    while (len < HUD_TEXT_MAX - 1 && text[len]) {
        g[len] = glyph(text[len]);
        len++;
    }
    w = hud_width(len);
    for (py = 0; py < h; py++) {
        int row = py - PAD;

        for (px = 0; px < w; px++)
            on[px] = 0;
        if (row >= 0 && row < CELL_H)
            for (i = 0; i < len; i++)
                if (g[i])
                    for (c = 0; c < 5 * SCALE; c++)
                        on[PAD + i * CELL_W + c] =
                            (unsigned char)((g[i][row / SCALE] >> (4 - c / SCALE)) & 1);
        if (format == 3) {
            unsigned int *p = (unsigned int *)fb + (y + py) * stride + x;
            for (px = 0; px < w; px++)
                p[px] = on[px] ? 0xFFFFFFFFu : 0xFF000000u;
        } else {
            unsigned short *p = (unsigned short *)fb + (y + py) * stride + x;
            for (px = 0; px < w; px++)
                p[px] = (unsigned short)(on[px] ? 0xFFFF : 0x0000);
        }
    }
}

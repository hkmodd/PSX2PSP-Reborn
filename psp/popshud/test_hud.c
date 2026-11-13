/*
 * PC test of hud.c: the measurements on known windows and the text drawn into a 480x272
 * buffer in every PSP pixel format. Writes hud_<format>.rgba (480x272, 8 bits per channel)
 * into the directory given as argument, for a look at the result.
 *   cl /W4 /WX /D_CRT_SECURE_NO_WARNINGS test_hud.c hud.c && test_hud <dir>
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "hud.h"

static int failures;

static void expect_text(unsigned int us, unsigned int flips, unsigned int cycles,
                        unsigned int idle, const char *want)
{
    HudStats s;
    char text[HUD_TEXT_MAX];
    int len;

    hud_compute(&s, us, flips, cycles, idle);
    len = hud_format(text, &s);
    if (strcmp(text, want) != 0 || len != (int)strlen(want) || len >= HUD_TEXT_MAX) {
        printf("FAIL us=%u flips=%u cycles=%u idle=%u: \"%s\", want \"%s\"\n", us, flips, cycles,
               idle, text, want);
        failures++;
    }
}

static unsigned int rgba(int format, const void *fb, int i)
{
    unsigned int v;

    if (format == 3)
        return ((const unsigned int *)fb)[i];
    v = ((const unsigned short *)fb)[i];
    return v == 0xFFFF ? 0xFFFFFFFFu : v == 0 ? 0xFF000000u : 0xFF0000FFu; /* red = stray */
}

int main(int argc, char **argv)
{
    static unsigned int fb32[512 * 272];
    static unsigned short fb16[512 * 272];
    const char *line = "FPS 30 EMU 100% CPU  85%";
    int format, i, lit = 0;

    /* One NTSC second: 59.94 vblanks of 564480 cycles. */
    expect_text(1000000, 30, 33868800, 150000, "FPS 30 EMU 100% CPU  85%");
    expect_text(1000000, 60, 33868800, 0, "FPS 60 EMU 100% CPU 100%");
    expect_text(1016000, 21, 25401600, 0, "FPS 21 EMU  74% CPU 100%");
    expect_text(1000000, 0, 0, 1000000, "FPS  0 EMU   0% CPU   0%");
    expect_text(1000000, 25, 0xF0000000u, 1300000, "FPS 25 EMU ---% CPU ---%");
    expect_text(0, 5, 5, 5, "FPS -- EMU ---% CPU ---%");
    expect_text(1000000, 120, 33868800, 1040000, "FPS 120 EMU 100% CPU   0%");

    if (hud_width((int)strlen(line)) > 480 - 16 || hud_height() > 272 - 16) {
        printf("FAIL box %dx%d does not fit\n", hud_width((int)strlen(line)), hud_height());
        failures++;
    }
    for (format = 0; format < 4; format++) {
        char path[512];
        FILE *f;
        void *fb = format == 3 ? (void *)fb32 : (void *)fb16;

        memset(fb32, 0x55, sizeof fb32);
        memset(fb16, 0x55, sizeof fb16);
        hud_draw(fb, 512, format, 8, 8, line);
        /* Nothing outside the box may change. */
        for (i = 0; i < 512 * 272; i++) {
            int x = i % 512, y = i / 512;
            int inside = x >= 8 && x < 8 + hud_width((int)strlen(line)) && y >= 8 &&
                         y < 8 + hud_height();
            unsigned int v = rgba(format, fb, i);
            int untouched = format == 3 ? v == 0x55555555u : ((unsigned short *)fb)[i] == 0x5555;
            if (inside == untouched) {
                printf("FAIL format %d pixel %d,%d\n", format, x, y);
                failures++;
                break;
            }
            if (inside && v == 0xFFFFFFFFu)
                lit++;
        }
        if (argc > 1) {
            snprintf(path, sizeof path, "%s/hud_%d.rgba", argv[1], format);
            f = fopen(path, "wb");
            if (!f)
                return 2;
            for (i = 0; i < 512 * 272; i++) {
                unsigned int v = rgba(format, fb, i), out;
                if (i % 512 >= 480)
                    continue;
                if (format != 3 && ((unsigned short *)fb)[i] == 0x5555)
                    v = 0xFF555555u;
                out = v; /* ABGR in memory = R, G, B, A bytes */
                fwrite(&out, 4, 1, f);
            }
            fclose(f);
        }
    }
    if (lit == 0) {
        printf("FAIL no lit pixel\n");
        failures++;
    }
    printf(failures ? "%d failures\n" : "ok, %d lit pixels per format\n", failures ? failures : lit / 4);
    return failures != 0;
}

/*
 * hud: the measurements and the text drawer of popshud, free of PSP calls so that the PC
 * test (test_hud.c) builds the same code.
 */
#ifndef POPSHUD_HUD_H
#define POPSHUD_HUD_H

/* A value that could not be measured; printed as dashes. */
#define HUD_NONE 0xFFFFFFFFu

/* Longest line hud_format writes, terminator included. */
#define HUD_TEXT_MAX 32

typedef struct {
    unsigned int fps; /* frames the game showed per second */
    unsigned int emu; /* emulated time over real time, percent */
    unsigned int cpu; /* time the PSP CPU spent outside its idle thread, percent */
} HudStats;

/*
 * One window of `us` real microseconds: `flips` changes of the game's display start,
 * `cycles` emulated CPU cycles, `idle_us` microseconds in the idle thread.
 */
void hud_compute(HudStats *s, unsigned int us, unsigned int flips, unsigned int cycles,
                 unsigned int idle_us);

/* Writes "FPS nn EMU nnn% CPU nnn%" into `out` (HUD_TEXT_MAX bytes); returns its length. */
int hud_format(char *out, const HudStats *s);

/* Width and height in pixels of `len` characters as hud_draw draws them, box included. */
int hud_width(int len);
int hud_height(void);

/*
 * Draws `text` in white on a black box whose top left corner is (x, y). `stride` is the
 * buffer width in pixels, `format` a PSP display pixel format: 0 = 565, 1 = 5551, 2 = 4444
 * (16 bits per pixel), 3 = 8888 (32 bits). Characters without a glyph are drawn as spaces.
 */
void hud_draw(void *fb, int stride, int format, int x, int y, const char *text);

#endif

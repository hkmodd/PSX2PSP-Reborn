/*
 * psxrt core: the CD-ROM controller (psx-spx "CDROM Controller"). Commands answer with INT3 after
 * the controller's acknowledge delay; commands with a second phase answer INT2 later; reads
 * deliver one INT1 per sector at 75/150 sectors per second. Responses wait in a small queue
 * while the guest has not acknowledged the previous interrupt, as the hardware holds them.
 * One data track (the disc image's), so GetTN/GetTD describe a single track. The shell opens and
 * closes for a disc change (cd_lid).
 */
#include "psx.h"

#include <string.h>

#define ACK_DELAY 50401u     /* psx-spx: Getstat with the motor on, 0xC4E1 */
#define INIT_ACK 81102u      /* 0x13CCE */
#define SEEK_TIME 100000u
#define ACK_GAP 1500u        /* a queued response follows an acknowledge after this */
#define SECTOR_1X (PSX_CLOCK / 75)
#define SPINUP_TIME PSX_CLOCK /* shell closed to motor on: DuckStation's one second */
/* The automatic disc change. A game that wants another disc tells the player so and polls
 * Getstat until the shell opens: Exoddus' "insert disc 2" polls 13 times a frame and sends
 * nothing else, against 0.4 a frame at its menu. Two one-second windows of Getstat alone at 4 a
 * frame or more, with the drive idle, open the shell; the next disc goes in after SWAP_OPEN and
 * the shell closes. A game still waiting gets the disc after it, up to every disc in turn; a
 * change long after the last one starts the round again. */
#define POLL_WINDOW PSX_CLOCK
#define POLL_HOT 240u /* Getstat per window: 4 a frame at 60 Hz */
#define SWAP_OPEN (2 * PSX_CLOCK)
#define SWAP_ROUND (30ull * PSX_CLOCK)

enum { ST_ERROR = 0x01, ST_MOTOR = 0x02, ST_SHELL = 0x10, ST_READ = 0x20, ST_SEEK = 0x40, ST_PLAY = 0x80 };

static u8 bcd(u32 v) { return (u8)((v / 10) << 4 | v % 10); }
static u32 unbcd(u8 v) { return (v >> 4) * 10 + (v & 15); }

static void msf(u32 lba, u8 *m)
{
    m[0] = bcd(lba / 4500), m[1] = bcd(lba / 75 % 60), m[2] = bcd(lba % 75);
}

static void xa_sector(Psx *psx, const u8 *raw);

static u32 period(const Cdrom *cd) { return (cd->mode & 0x80) ? SECTOR_1X / 2 : SECTOR_1X; }

static CdResp *push(Psx *psx, u8 irq, u64 at)
{
    Cdrom *cd = &psx->cd;
    if (cd->nq == (int)(sizeof cd->q / sizeof cd->q[0])) {
        psx_log(psx, "cd: response queue full, INT%d dropped", cd->q[0].irq);
        memmove(cd->q, cd->q + 1, sizeof cd->q[0] * (size_t)(cd->nq - 1));
        cd->nq--;
    }
    CdResp *r = &cd->q[cd->nq++];
    memset(r, 0, sizeof *r);
    r->irq = irq;
    if (cd->nq == 1 || at > cd->q_due)
        cd->q_due = at;
    psx_schedule(psx, cd->q_due);
    return r;
}

static void resp_stat(Psx *psx, u8 irq, u64 at)
{
    CdResp *r = push(psx, irq, at);
    r->b[0] = psx->cd.stat, r->n = 1;
}

static void deliver(Psx *psx)
{
    Cdrom *cd = &psx->cd;
    CdResp *r = &cd->q[0];
    memcpy(cd->resp, r->b, r->n);
    cd->nresp = r->n, cd->rpos = 0;
    cd->irq_flags = (u8)((cd->irq_flags & ~7u) | r->irq);
    if (r->sector) {
        memcpy(cd->sector, cd->pending, sizeof cd->sector);
        cd->n_unread += (u32)cd->fresh;
        cd->fresh = 1;
    }
    memmove(cd->q, cd->q + 1, sizeof cd->q[0] * (size_t)(cd->nq - 1));
    cd->nq--;
    if (psx->trace & 2)
        psx_log(psx, "cd INT%d %02X %02X (%d bytes) mask %02X msf %02X:%02X:%02X at %u", cd->irq_flags & 7, cd->resp[0], cd->resp[1], cd->nresp,
                cd->irq_mask, cd->sector[12], cd->sector[13], cd->sector[14], (unsigned)psx->cycles);
    if (cd->irq_flags & cd->irq_mask & 0x1F)
        psx_raise(psx, IRQ_CDROM);
}

static void stop_reading(Cdrom *cd)
{
    cd->reading = 0;
    cd->read_due = 0;
    cd->stat &= (u8)~(ST_READ | ST_SEEK | ST_PLAY);
}

static void start_read(Psx *psx)
{
    Cdrom *cd = &psx->cd;
    u32 delay = period(cd);
    if (cd->setloc_pending) {
        u32 dist = cd->setloc > cd->pos ? cd->setloc - cd->pos : cd->pos - cd->setloc;
        if (dist > 8)
            delay += SEEK_TIME;
        cd->pos = cd->setloc;
        cd->setloc_pending = 0;
    }
    cd->reading = 1;
    cd->stat = (u8)((cd->stat & ~(ST_SEEK | ST_PLAY)) | ST_MOTOR | ST_READ);
    cd->read_due = psx->cycles + ACK_DELAY + delay;
    if (cd->lid == CD_LID_SPINUP && cd->read_due < cd->lid_due + delay)
        cd->read_due = cd->lid_due + delay;
    psx_schedule(psx, cd->read_due);
}

/* One sector of a read came under the head. */
static void sector(Psx *psx)
{
    Cdrom *cd = &psx->cd;
    u8 raw[2352];
    cd->read_due += period(cd);
    if (cd->read_due <= psx->cycles)
        cd->read_due = psx->cycles + period(cd);
    /* a sector the XA filter drops is not read past its subheader */
    const PsxHost *h = psx->host;
    int got = h->read_head ? h->read_head(h->user, cd->pos, raw) : -2;
    if (got == 0 && (cd->mode & 0x08) && (raw[18] & 0x04) && (raw[18] & 0x40) && raw[15] == 2 &&
        (raw[16] != cd->filter_file || raw[17] != cd->filter_chan)) {
        cd->pos++;
        return; /* filtered out, as below */
    }
    if (got == -1 || !h->read_sector || h->read_sector(h->user, cd->pos, raw) < 0) {
        psx_log(psx, "cd: read past the end at LBA %u", cd->pos);
        stop_reading(cd);
        cd->stat |= ST_ERROR;
        CdResp *r = push(psx, 5, psx->cycles);
        r->b[0] = cd->stat, r->b[1] = 0x04, r->n = 2;
        return;
    }
    cd->pos++;
    u8 file = raw[16], chan = raw[17], sub = raw[18];
    int xa_audio = (sub & 0x04) && (sub & 0x40) && raw[15] == 2;
    if ((cd->mode & 0x08) && xa_audio && (file != cd->filter_file || chan != cd->filter_chan))
        return; /* filtered out */
    if ((cd->mode & 0x40) && xa_audio) { /* XA-ADPCM: to the SPU, no INT1 */
        xa_sector(psx, raw);
        cd->n_xa++;
        return;
    }
    memcpy(cd->pending, raw, sizeof raw);
    cd->n_sect++;
    for (int i = 0; i < cd->nq; i++)
        if (cd->q[i].sector) {
            cd->n_replaced++;
            if (psx->trace & 2)
                psx_log(psx, "cd: sector %u replaces the waiting one", cd->pos - 1);
            return; /* the guest is behind: the newer sector replaces the waiting one */
        }
    CdResp *r = push(psx, 1, psx->cycles);
    r->b[0] = cd->stat, r->n = 1;
    r->sector = 1;
}

static void second(Psx *psx)
{
    Cdrom *cd = &psx->cd;
    u8 c = cd->cmd2;
    cd->cmd2 = 0;
    switch (c) {
    case 0x1A: { /* GetID: licensed Mode 2 disc, region from the boot EXE */
        CdResp *r = push(psx, 2, psx->cycles);
        static const u8 id[8] = {0x02, 0x00, 0x20, 0x00, 'S', 'C', 'E', 'E'};
        memcpy(r->b, id, 8);
        if (!psx->pal)
            r->b[7] = 'A';
        r->n = 8;
        return;
    }
    case 0x09: /* Pause */
    case 0x08: /* Stop */
        stop_reading(cd);
        if (c == 0x08)
            cd->stat &= (u8)~ST_MOTOR;
        break;
    case 0x15: case 0x16: /* SeekL/P */
        cd->stat &= (u8)~ST_SEEK;
        break;
    }
    resp_stat(psx, 2, psx->cycles);
}

/* Counts the waiting polls of the automatic disc change (see POLL_WINDOW). */
static void swap_poll(Psx *psx, u8 c)
{
    Cdrom *cd = &psx->cd;
    const PsxHost *h = psx->host;
    if (!h->auto_swap || !h->disc_select || h->discs < 2 || cd->lid != CD_LID_CLOSED || cd->swap_on)
        return;
    if (c != 0x01 || (cd->stat & (ST_READ | ST_SEEK | ST_PLAY))) {
        cd->poll_n = 0, cd->poll_hot = 0, cd->poll_t0 = psx->cycles;
        return;
    }
    cd->poll_n++;
    u64 dt = psx->cycles - cd->poll_t0;
    if (dt < POLL_WINDOW)
        return;
    cd->poll_hot = (u64)cd->poll_n * POLL_WINDOW >= (u64)POLL_HOT * dt ? cd->poll_hot + 1 : 0;
    cd->poll_n = 0, cd->poll_t0 = psx->cycles;
    if (cd->poll_hot < 2)
        return;
    cd->poll_hot = 0;
    if (psx->cycles - cd->swap_due > SWAP_ROUND)
        cd->swap_tries = 0;
    if (cd->swap_tries >= h->discs)
        return; /* no disc stopped the polling: the game waits for something else */
    cd->swap_tries++;
    cd->swap_on = 1, cd->swap_due = psx->cycles + SWAP_OPEN;
    psx_log(psx, "cd: the game polls the drive, waiting for a disc: automatic change");
    cd_lid(psx, 1);
    psx_schedule(psx, cd->swap_due);
}

static void command(Psx *psx, u8 c)
{
    Cdrom *cd = &psx->cd;
    const u8 *p = cd->params;
    swap_poll(psx, c);
    u64 ack = psx->cycles + ACK_DELAY;
    u32 later = 0; /* delay of the second response after the first */
    if (psx->trace & 2)
        psx_log(psx, "cd %02X %02X %02X %02X (%d) stat %02X mode %02X", c, p[0], p[1], p[2], cd->nparams, cd->stat, cd->mode);
    cd->stat &= (u8)~ST_ERROR;
    if (cd->lid == CD_LID_OPEN && (c == 0x03 || c == 0x06 || c == 0x07 || c == 0x15 || c == 0x16 || c == 0x1A || c == 0x1B || c == 0x1E)) {
        CdResp *r = push(psx, 5, ack); /* Play, ReadN, MotorOn, SeekL/P, GetID, ReadS, ReadTOC: no disc */
        r->b[0] = cd->stat | ST_ERROR, r->b[1] = 0x80, r->n = 2;
        cd->nparams = 0;
        return;
    }
    switch (c) {
    case 0x01: /* Getstat */
        resp_stat(psx, 3, ack);
        if (cd->lid != CD_LID_OPEN)
            cd->stat &= (u8)~ST_SHELL; /* shell-open latch clears once reported with the shell closed */
        break;
    case 0x02: /* Setloc */
        cd->setloc = (unbcd(p[0]) * 60 + unbcd(p[1])) * 75 + unbcd(p[2]);
        cd->setloc_pending = 1;
        resp_stat(psx, 3, ack);
        break;
    case 0x03: /* Play: no audio tracks on this disc */
        resp_stat(psx, 3, ack);
        break;
    case 0x06: case 0x1B: /* ReadN, ReadS */
        resp_stat(psx, 3, ack);
        start_read(psx);
        break;
    case 0x07: /* MotorOn */
        cd->stat |= ST_MOTOR;
        resp_stat(psx, 3, ack);
        later = 0x4000;
        break;
    case 0x08: /* Stop */
        resp_stat(psx, 3, ack);
        later = cd->reading ? 0x10BD93 : 0x1000;
        break;
    case 0x09: /* Pause */
        resp_stat(psx, 3, ack);
        later = cd->reading ? ((cd->mode & 0x80) ? 0x10BD93u : 0x21181Cu) : 0x1DF2u;
        cd->reading = 0, cd->read_due = 0; /* no sector after the command */
        break;
    case 0x0A: /* Init */
        stop_reading(cd);
        cd->mode = 0;
        cd->stat = cd->lid == CD_LID_OPEN ? ST_SHELL : ST_MOTOR;
        resp_stat(psx, 3, psx->cycles + INIT_ACK);
        later = 0x2000;
        ack = psx->cycles + INIT_ACK;
        break;
    case 0x0B: spu_sync(psx), cd->muted = 1; resp_stat(psx, 3, ack); break;
    case 0x0C: spu_sync(psx), cd->muted = 0; resp_stat(psx, 3, ack); break;
    case 0x0D: /* Setfilter */
        cd->filter_file = p[0], cd->filter_chan = p[1];
        resp_stat(psx, 3, ack);
        break;
    case 0x0E: /* Setmode */
        cd->mode = p[0];
        resp_stat(psx, 3, ack);
        break;
    case 0x0F: { /* Getparam */
        CdResp *r = push(psx, 3, ack);
        r->b[0] = cd->stat, r->b[1] = cd->mode, r->b[2] = 0, r->b[3] = cd->filter_file, r->b[4] = cd->filter_chan;
        r->n = 5;
        break;
    }
    case 0x10: { /* GetlocL: header and subheader of the last sector */
        CdResp *r = push(psx, 3, ack);
        memcpy(r->b, cd->sector + 12, 8);
        r->n = 8;
        break;
    }
    case 0x11: { /* GetlocP */
        CdResp *r = push(psx, 3, ack);
        u32 at = cd->pos ? cd->pos - 1 : 0;
        r->b[0] = 0x01, r->b[1] = 0x01;
        msf(at >= 150 ? at - 150 : 0, r->b + 2);
        msf(at, r->b + 5);
        r->n = 8;
        break;
    }
    case 0x12: /* SetSession */
        resp_stat(psx, 3, ack);
        later = 0x4000;
        break;
    case 0x13: { /* GetTN */
        CdResp *r = push(psx, 3, ack);
        r->b[0] = cd->stat, r->b[1] = 0x01, r->b[2] = 0x01, r->n = 3;
        break;
    }
    case 0x14: { /* GetTD */
        u32 t = unbcd(p[0]);
        CdResp *r = push(psx, 3, ack);
        if (t > 1) {
            r->b[0] = cd->stat | ST_ERROR, r->b[1] = 0x10, r->n = 2;
            r->irq = 5;
            break;
        }
        u8 m[3];
        msf(t == 0 ? psx->host->disc_end : 150, m);
        r->b[0] = cd->stat, r->b[1] = m[0], r->b[2] = m[1], r->n = 3;
        break;
    }
    case 0x15: case 0x16: /* SeekL, SeekP */
        stop_reading(cd);
        cd->pos = cd->setloc;
        cd->setloc_pending = 0;
        cd->stat |= ST_SEEK | ST_MOTOR;
        resp_stat(psx, 3, ack);
        later = SEEK_TIME;
        break;
    case 0x19: { /* Test */
        CdResp *r = push(psx, 3, ack);
        if (p[0] == 0x20) {
            static const u8 ver[4] = {0x94, 0x09, 0x19, 0xC0};
            memcpy(r->b, ver, 4);
            r->n = 4;
        } else
            r->b[0] = cd->stat, r->n = 1;
        break;
    }
    case 0x1A: /* GetID */
        resp_stat(psx, 3, ack);
        later = 0x4A00;
        break;
    case 0x1E: /* ReadTOC */
        resp_stat(psx, 3, ack);
        later = 0x100000;
        break;
    default: {
        psx_log(psx, "cd: unknown command %02X", c);
        CdResp *r = push(psx, 5, ack);
        r->b[0] = cd->stat | ST_ERROR, r->b[1] = 0x40, r->n = 2;
        break;
    }
    }
    if (later) {
        cd->cmd2 = c;
        cd->cmd2_due = ack + later;
        if (cd->lid == CD_LID_SPINUP && cd->cmd2_due < cd->lid_due + later)
            cd->cmd2_due = cd->lid_due + later; /* the drive answers once spun up */
        psx_schedule(psx, cd->cmd2_due);
    }
    cd->nparams = 0;
}

void cd_lid(Psx *psx, int open)
{
    Cdrom *cd = &psx->cd;
    if (open) {
        if (cd->lid == CD_LID_OPEN)
            return;
        cd->lid = CD_LID_OPEN;
        stop_reading(cd);
        cd->cmd2 = 0;
        int k = 0; /* sectors still waiting are the old disc's */
        for (int i = 0; i < cd->nq; i++)
            if (!cd->q[i].sector)
                cd->q[k++] = cd->q[i];
        cd->nq = k;
        cd->stat = ST_SHELL | ST_ERROR; /* psx-spx: every bit but shell open and error clears */
        CdResp *r = push(psx, 5, psx->cycles);
        r->b[0] = cd->stat, r->b[1] = 0x08, r->n = 2;
        psx_log(psx, "cd: shell open");
    } else if (cd->lid == CD_LID_OPEN) {
        cd->lid = CD_LID_SPINUP;
        cd->lid_due = psx->cycles + SPINUP_TIME;
        psx_schedule(psx, cd->lid_due);
        psx_log(psx, "cd: shell closed, disc end LBA %u", psx->host->disc_end);
    }
}

int cd_insert(Psx *psx, u32 n)
{
    const PsxHost *h = psx->host;
    if (!h->disc_select || n >= h->discs || !h->disc_select(h->user, n))
        return 0;
    psx->cd.disc = (u8)n;
    return 1;
}

u8 cd_read(Psx *psx, int reg)
{
    Cdrom *cd = &psx->cd;
    switch (reg) {
    case 0:
        return (u8)(cd->index | (cd->nparams == 0) << 3 | (cd->nparams < CD_FIFO) << 4 | (cd->rpos < cd->nresp) << 5 |
                    (cd->data_pos < cd->data_len) << 6);
    case 1:
        return cd->rpos < cd->nresp ? cd->resp[cd->rpos++] : 0;
    case 2:
        if (cd->data_pos < cd->data_len)
            return cd->data[cd->data_pos++];
        return cd->data_len ? cd->data[cd->data_len - 8 + (cd->data_pos++ & 7)] : 0; /* past the end */
    default:
        return (u8)(((cd->index & 1) ? cd->irq_flags : cd->irq_mask) | 0xE0);
    }
}

void cd_write(Psx *psx, int reg, u8 v)
{
    Cdrom *cd = &psx->cd;
    int idx = cd->index;
    if (reg == 0) {
        cd->index = v & 3;
        return;
    }
    switch (reg << 2 | idx) {
    case 1 << 2 | 0: command(psx, v); break;
    case 1 << 2 | 3: cd->vol_next[3] = v; break;
    case 2 << 2 | 0:
        if (cd->nparams < CD_FIFO)
            cd->params[cd->nparams++] = v;
        break;
    case 2 << 2 | 1: { /* the interrupt line is (flags & mask): enabling a pending flag raises it */
        int was = cd->irq_flags & cd->irq_mask & 0x1F;
        cd->irq_mask = v & 0x1F;
        if (!was && (cd->irq_flags & cd->irq_mask & 0x1F))
            psx_raise(psx, IRQ_CDROM);
        break;
    }
    case 2 << 2 | 2: cd->vol_next[0] = v; break;
    case 2 << 2 | 3: cd->vol_next[2] = v; break;
    case 3 << 2 | 0: /* request: BFRD loads the sector buffer, clearing it empties the FIFO */
        if (v & 0x80) {
            if (cd->data_pos >= cd->data_len) {
                int raw = cd->mode & 0x20;
                cd->data_len = raw ? 2340 : 2048;
                memcpy(cd->data, cd->sector + (raw ? 12 : 24), (size_t)cd->data_len);
                cd->data_pos = 0;
                cd->n_loads += (u32)cd->fresh;
                cd->fresh = 0;
            }
        } else
            cd->data_len = cd->data_pos = 0;
        break;
    case 3 << 2 | 1: /* acknowledge */
        cd->irq_flags &= (u8)~(v & 0x1F);
        if (v & 0x40)
            cd->nparams = 0;
        if (!(cd->irq_flags & 7) && cd->nq) {
            if (cd->q_due < psx->cycles + ACK_GAP)
                cd->q_due = psx->cycles + ACK_GAP;
            psx_schedule(psx, cd->q_due);
        }
        break;
    case 3 << 2 | 2: cd->vol_next[1] = v; break;
    case 3 << 2 | 3:
        if (v & 0x20) {
            spu_sync(psx); /* the new volumes from this cycle on */
            memcpy(cd->vol, cd->vol_next, 4);
        }
        break;
    }
}

void cd_update(Psx *psx)
{
    Cdrom *cd = &psx->cd;
    if (cd->lid == CD_LID_SPINUP && psx->cycles >= cd->lid_due)
        cd->lid = CD_LID_CLOSED, cd->stat |= ST_MOTOR;
    if (cd->swap_on && psx->cycles >= cd->swap_due) {
        cd->swap_on = 0, cd->swap_due = psx->cycles;
        if (cd->lid == CD_LID_OPEN) { /* still ours: the player did not close it meanwhile */
            u32 n = (cd->disc + 1u) % psx->host->discs;
            if (cd_insert(psx, n))
                psx_log(psx, "cd: disc %u in the drive", (unsigned)n + 1);
            cd_lid(psx, 0);
        }
    }
    if (cd->cmd2 && psx->cycles >= cd->cmd2_due)
        second(psx);
    if (cd->reading && psx->cycles >= cd->read_due)
        sector(psx);
    if (cd->nq && !(cd->irq_flags & 7) && psx->cycles >= cd->q_due)
        deliver(psx);
    if (cd->cmd2)
        psx_schedule(psx, cd->cmd2_due);
    if (cd->reading)
        psx_schedule(psx, cd->read_due);
    if (cd->nq && !(cd->irq_flags & 7))
        psx_schedule(psx, cd->q_due);
    if (cd->lid == CD_LID_SPINUP)
        psx_schedule(psx, cd->lid_due);
    if (cd->swap_on)
        psx_schedule(psx, cd->swap_due);
}

/* A DMA's whole words of the sector's data, straight into `dst`: up to `n`, while the data lasts
 * (cd_dma_word's bytes, in the same order); 0 when less than a word is left, for cd_dma_word. */
u32 cd_dma_block(Psx *psx, u32 *dst, u32 n)
{
    Cdrom *cd = &psx->cd;
    u32 left = cd->data_pos < cd->data_len ? (u32)(cd->data_len - cd->data_pos) / 4 : 0;
    if (n > left)
        n = left;
    memcpy(dst, cd->data + cd->data_pos, 4 * n);
    cd->data_pos += (int)(4 * n);
    return n;
}

u32 cd_dma_word(Psx *psx)
{
    u32 v = 0;
    for (int i = 0; i < 4; i++)
        v |= (u32)cd_read(psx, 2) << (8 * i);
    return v;
}

static s32 clamp16(s32 x) { return x < -0x8000 ? -0x8000 : x > 0x7FFF ? 0x7FFF : x; }

/* The XA play cursor (cd_audio) runs on the SPU's sample clock, one sample each. A sector that
 * finds the cursor dry (a stream starts, or the drive fell behind) plays CD_XA_LEAD samples after
 * its arrival; the sectors after it follow on, so the delay from a sector's arrival to its sound is
 * the same through a film and does not grow with each underflow. The cursor stops CD_XA_LOST
 * samples past the last one. Every sector arrives after the SPU ran up to its cycle, so what plays
 * depends on cycles only, not on where the mix is cut. */
#define CD_XA_LEAD 1176 /* 26.7 ms: half a sector of 37.8 kHz stereo */
#define CD_XA_LOST (CD_XA_FIFO / 2)

static void xa_stop(Cdrom *cd)
{
    cd->xa_on = 0;
    cd->xa_rd = cd->xa_wr;
}

static void xa_push(Cdrom *cd, s32 l, s32 r)
{
    if (cd->xa_wr - cd->xa_rd >= CD_XA_FIFO)
        return; /* the SPU is not consuming: drop rather than grow the delay */
    u32 i = cd->xa_wr++ % CD_XA_FIFO;
    cd->xa_fifo[2 * i] = (s16)l, cd->xa_fifo[2 * i + 1] = (s16)r;
}

/* An XA-ADPCM sector (psx-spx "CDROM XA Audio ADPCM Compression"): 18 sound groups of 128 bytes,
 * each eight 28-sample units (4-bit) after a 16-byte header whose bytes 4..11 hold the units'
 * shift and filter; stereo alternates left and right units. Resampled to 44100 Hz by linear
 * interpolation (the hardware's 7-tap zigzag filter is not reproduced). Follows RecompOne's
 * XaAudio.cs (MIT, notice in spu.c). */
static void xa_sector(Psx *psx, const u8 *raw)
{
    static const s32 k0[4] = {0, 60, 115, 98}, k1[4] = {0, 0, -52, -55};
    Cdrom *cd = &psx->cd;
    u8 ci = raw[19];
    int stereo = (ci & 3) == 1, rate = (ci >> 2 & 3) == 1 ? 18900 : 37800;
    if ((ci >> 4 & 3) != 0) {
        static int told;
        if (!told++)
            psx_log(psx, "cd: 8-bit XA-ADPCM not supported (coding %02X)", ci);
        return;
    }
    if (psx->trace & 64) {
        static u32 seen;
        if (!(seen++ & 31)) {
            const u16 *g = psx->spu.regs;
            psx_log(psx, "xa: %u sectors, file %u chan %u coding %02X, fifo %u, muted %d, cd vol %u %u %u %u, spucnt %04X cdvol %04X/%04X main %04X/%04X",
                    (unsigned)seen, raw[16], raw[17], ci, (unsigned)(cd->xa_wr - cd->xa_rd), cd->muted, cd->vol[0], cd->vol[1],
                    cd->vol[2], cd->vol[3], psx->spu.ctrl, g[(0x1F801DB0 - 0x1F801C00) >> 1], g[(0x1F801DB2 - 0x1F801C00) >> 1],
                    g[(0x1F801D80 - 0x1F801C00) >> 1], g[(0x1F801D82 - 0x1F801C00) >> 1]);
        }
    }
    static s16 pcm[2][4032];
    int n[2] = {0, 0};
    for (int g = 0; g < 18; g++) {
        const u8 *grp = raw + 24 + g * 128;
        for (int u = 0; u < 8; u++) {
            int ch = stereo ? u & 1 : 0, shift = grp[4 + u] & 15, f = grp[4 + u] >> 4 & 3;
            if (shift > 12)
                shift = 9;
            s32 h0 = cd->xa_prev[ch][0], h1 = cd->xa_prev[ch][1];
            const u8 *d = grp + 16 + (u >> 1);
            int nib = (u & 1) * 4;
            for (int j = 0; j < 28; j++) {
                s32 x = ((s32)(s16)((d[j * 4] >> nib & 15) << 12) >> shift) + ((h0 * k0[f] + h1 * k1[f] + 32) >> 6);
                x = clamp16(x);
                h1 = h0, h0 = x;
                pcm[ch][n[ch]++] = (s16)x;
            }
            cd->xa_prev[ch][0] = h0, cd->xa_prev[ch][1] = h1;
        }
    }
    spu_sync(psx); /* the samples before this sector's cycle are mixed without it */
    if (!cd->xa_on || (s32)(cd->xa_wr - cd->xa_rd) < 0) { /* a stream starts, or goes on after it ran dry */
        xa_stop(cd);
        cd->xa_on = 1;
        for (int i = 0; i < CD_XA_LEAD; i++)
            xa_push(cd, 0, 0);
    }
    u32 step = (u32)(((u64)rate << 16) / 44100);
    for (int i = 0; i < n[0]; i++) {
        s32 l = pcm[0][i], r = stereo ? pcm[1][i] : l;
        for (; cd->xa_phase < 0x10000; cd->xa_phase += step) {
            s32 t = (s32)(cd->xa_phase >> 1); /* 15 bits keep the product in range */
            xa_push(cd, cd->xa_last[0] + ((l - cd->xa_last[0]) * t >> 15), cd->xa_last[1] + ((r - cd->xa_last[1]) * t >> 15));
        }
        cd->xa_phase -= 0x10000;
        cd->xa_last[0] = (s16)l, cd->xa_last[1] = (s16)r;
    }
}

void cd_audio(Psx *psx, s32 *l, s32 *r, int n)
{
    Cdrom *cd = &psx->cd;
    const s32 v0 = cd->vol[0], v1 = cd->vol[1], v2 = cd->vol[2], v3 = cd->vol[3];
    const int muted = cd->muted; /* the samples are used up all the same */
    for (int k = 0; k < n; k++) {
        s32 a = 0, b = 0;
        if (cd->xa_on) {
            u32 rd = cd->xa_rd++;
            s32 lead = (s32)(cd->xa_wr - rd);
            if (lead > 0) {
                u32 i = rd % CD_XA_FIFO;
                if (!muted)
                    a = cd->xa_fifo[2 * i], b = cd->xa_fifo[2 * i + 1];
            } else if (-lead >= CD_XA_LOST) {
                xa_stop(cd);
            }
        }
        l[k] = clamp16((a * v0 + b * v2) >> 7); /* LL, RL: 0x80 is 100% */
        r[k] = clamp16((a * v1 + b * v3) >> 7); /* LR, RR */
    }
}

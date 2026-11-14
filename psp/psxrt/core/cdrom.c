/*
 * psxrt core: the CD-ROM controller (psx-spx "CDROM Controller"). Commands answer with INT3 after
 * the controller's acknowledge delay; commands with a second phase answer INT2 later; reads
 * deliver one INT1 per sector at 75/150 sectors per second. Responses wait in a small queue
 * while the guest has not acknowledged the previous interrupt, as the hardware holds them.
 * One data track (the disc image's), so GetTN/GetTD describe a single track.
 */
#include "psx.h"

#include <string.h>

#define ACK_DELAY 50401u     /* psx-spx: Getstat with the motor on, 0xC4E1 */
#define INIT_ACK 81102u      /* 0x13CCE */
#define SEEK_TIME 100000u
#define ACK_GAP 1500u        /* a queued response follows an acknowledge after this */
#define SECTOR_1X (PSX_CLOCK / 75)

enum { ST_ERROR = 0x01, ST_MOTOR = 0x02, ST_READ = 0x20, ST_SEEK = 0x40, ST_PLAY = 0x80 };

static u8 bcd(u32 v) { return (u8)((v / 10) << 4 | v % 10); }
static u32 unbcd(u8 v) { return (v >> 4) * 10 + (v & 15); }

static void msf(u32 lba, u8 *m)
{
    m[0] = bcd(lba / 4500), m[1] = bcd(lba / 75 % 60), m[2] = bcd(lba % 75);
}

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
    if (r->sector)
        memcpy(cd->sector, cd->pending, sizeof cd->sector);
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
    if (!psx->host->read_sector || psx->host->read_sector(psx->host->user, cd->pos, raw) < 0) {
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
    if ((cd->mode & 0x40) && xa_audio)
        return; /* XA-ADPCM goes to the audio path (S6) */
    memcpy(cd->pending, raw, sizeof raw);
    for (int i = 0; i < cd->nq; i++)
        if (cd->q[i].sector) {
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

static void command(Psx *psx, u8 c)
{
    Cdrom *cd = &psx->cd;
    const u8 *p = cd->params;
    u64 ack = psx->cycles + ACK_DELAY;
    u32 later = 0; /* delay of the second response after the first */
    if (psx->trace & 2)
        psx_log(psx, "cd %02X %02X %02X %02X (%d) stat %02X mode %02X", c, p[0], p[1], p[2], cd->nparams, cd->stat, cd->mode);
    cd->stat &= (u8)~ST_ERROR;
    switch (c) {
    case 0x01: /* Getstat */
        resp_stat(psx, 3, ack);
        cd->stat &= (u8)~0x10; /* shell-open latch clears once reported */
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
        cd->stat = ST_MOTOR;
        resp_stat(psx, 3, psx->cycles + INIT_ACK);
        later = 0x2000;
        ack = psx->cycles + INIT_ACK;
        break;
    case 0x0B: cd->muted = 1; resp_stat(psx, 3, ack); break;
    case 0x0C: cd->muted = 0; resp_stat(psx, 3, ack); break;
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
        psx_schedule(psx, cd->cmd2_due);
    }
    cd->nparams = 0;
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
        if (v & 0x20)
            memcpy(cd->vol, cd->vol_next, 4);
        break;
    }
}

void cd_update(Psx *psx)
{
    Cdrom *cd = &psx->cd;
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
}

u32 cd_dma_word(Psx *psx)
{
    u32 v = 0;
    for (int i = 0; i < 4; i++)
        v |= (u32)cd_read(psx, 2) << (8 * i);
    return v;
}

void cd_mix(Psx *psx, s32 *l, s32 *r, int n44100)
{
    (void)psx, (void)l, (void)r, (void)n44100; /* XA playback arrives with S6 */
}

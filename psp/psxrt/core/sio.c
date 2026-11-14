/*
 * psxrt core: SIO0, the controller/memory card port, with a digital pad in port 1. Each byte the
 * guest sends gets the pad's reply at once and an /ACK interrupt (IRQ7) a little later, except
 * after the last byte. Memory cards go through the BIOS HLE, so card selects are not answered.
 * SIO1 (serial) is absent.
 */
#include "psx.h"

#define ACK_TIME 1500u /* 8 bits at the usual baud (0x88) plus the pad's /ACK delay */

static u8 pad_byte(Psx *psx, u8 tx, int *ack)
{
    Sio *s = &psx->sio;
    u16 b = s->buttons[0];
    *ack = 1;
    switch (s->step++) {
    case 0:
        if (tx != 0x01) {
            s->device = 0, *ack = 0; /* 0x81: memory card, not on this bus */
            return 0xFF;
        }
        s->device = 1;
        return 0xFF;
    case 1: return tx == 0x42 ? 0x41 : (s->device = 0, *ack = 0, 0xFF);
    case 2: return 0x5A;
    case 3: return (u8)b;
    default:
        *ack = 0;
        s->device = 0;
        return (u8)(b >> 8);
    }
}

u32 sio_read(Psx *psx, u32 addr, int size)
{
    Sio *s = &psx->sio;
    (void)size;
    switch (addr & 0x1F) {
    case 0x00: {
        u8 v = s->rx_full ? s->rx : 0xFF;
        s->rx_full = 0;
        return v;
    }
    case 0x04: return 0x5u | (u32)s->rx_full << 1 | (s->stat & 0x200);
    case 0x08: return s->mode;
    case 0x0A: return s->ctrl;
    case 0x0E: return s->baud;
    case 0x14: return 0x5; /* SIO1 status: TX ready, nothing received */
    }
    return 0;
}

void sio_write(Psx *psx, u32 addr, u32 v, int size)
{
    Sio *s = &psx->sio;
    (void)size;
    switch (addr & 0x1F) {
    case 0x00: {
        int ack = 0;
        if ((s->ctrl & 0x2) && !(s->ctrl & 0x2000) && (s->step == 0 || s->device == 1))
            s->rx = pad_byte(psx, (u8)v, &ack);
        else
            s->rx = 0xFF;
        s->rx_full = 1;
        if (ack) {
            s->ack_due = psx->cycles + ACK_TIME;
            psx_schedule(psx, s->ack_due);
        }
        break;
    }
    case 0x08: s->mode = (u16)v; break;
    case 0x0A:
        if (v & 0x40) { /* reset */
            s->stat = 0, s->rx_full = 0, s->ack_due = 0;
            v = 0;
        }
        if (v & 0x10)
            s->stat &= (u16)~0x200; /* acknowledge */
        if (!(v & 0x2)) /* select released: the transaction ends */
            s->step = 0, s->device = 0;
        s->ctrl = (u16)(v & ~0x50u);
        break;
    case 0x0E: s->baud = (u16)v; break;
    }
}

void sio_update(Psx *psx)
{
    Sio *s = &psx->sio;
    if (!s->ack_due)
        return;
    if (psx->cycles < s->ack_due) {
        psx_schedule(psx, s->ack_due);
        return;
    }
    s->ack_due = 0;
    if (s->ctrl & 0x1000) {
        s->stat |= 0x200;
        psx_raise(psx, IRQ_SIO0);
    }
}

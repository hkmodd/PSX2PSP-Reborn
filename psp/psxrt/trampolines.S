/*
 * psxrt: the doors between translated code and the runtime (crates/psx2psp-recomp, emit.rs).
 *
 * Translated code keeps the guest's registers in the host's (identity), HI/LO in HI/LO, k0 =
 * K0 and the guest's k0/k1 in CTX. The stubs at RT_CALL/MISS/CALL_RET/GRET (written by
 * native.c) jump here; each saves the guest state into CTX, switches to the host stack and
 * gp, calls native.c, then restores the guest state and jumps to the host address it got.
 *
 *   RT_CALL   host call: `sw ra,GPR31(k0); jal RT_CALL; nop; .word pc; .word insn`, so ra is
 *             the address of the two words and the guest's ra is already in CTX.
 *   MISS      dispatch-table miss: the guest target is in CTX.TARGET.
 *   GRET      return to a guest address the translation lacks (native.c, fix_ra).
 *   CALL_RET  end of a guest call made with native_enter(): returns to its C caller.
 */
    .set noreorder
    .set noat

#define K0_VALUE 0x09A0004C
#define G(r) (0x3B4 + 4 * (r)) /* CTX GPR r from k0 */
#define HI 0x434
#define LO 0x438
#define BUDGET 0x444 /* CTX.BUDGET (native.c C_BUDGET) */
/* CTX.GTE_RET: one slot for every lean entry (psxrt_gte_*, psxrt_io_read/write): each parks its
 * return site here and the C behind it (C_SITE in io_fast.c, native.c) reads it once on entry.
 * That holds only while no lean entry runs translated code (run_guest) before reading C_SITE: a
 * nested entry would overwrite the outer one's return. */
#define GTE_RET 0x448

/* frame of native_enter: 16 bytes for the callee, s0-s7, fp, ra, gp, previous host sp */
#define F_S0 16
#define F_FP 48
#define F_RA 52
#define F_GP 56
#define F_PREV 60
#define FRAME 64

.macro SAVE_GUEST
    .irp r,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,28,29,30
    sw $\r, G(\r)($k0)
    .endr
    mfhi $k1
    sw $k1, HI($k0)
    mflo $k1
    sw $k1, LO($k0)
.endm

/* Host stack and gp; k1 cleared for the kernel's syscall entry. */
.macro TO_HOST
    lui $k1, %hi(native_host_sp)
    lw $sp, %lo(native_host_sp)($k1)
    lui $k1, %hi(native_host_gp)
    lw $gp, %lo(native_host_gp)($k1)
    move $k1, $zero
.endm

    .text

    .globl psxrt_rt_call
    .ent psxrt_rt_call
psxrt_rt_call:
    SAVE_GUEST
    move $a0, $ra
    TO_HOST
    jal native_rt_call
    nop
    b restore_and_go
    move $k1, $v0
    .end psxrt_rt_call

    .globl psxrt_miss
    .ent psxrt_miss
psxrt_miss:
    SAVE_GUEST
    sw $ra, G(31)($k0)
    TO_HOST
    jal native_miss
    nop
    b restore_and_go
    move $k1, $v0
    .end psxrt_miss

    .globl psxrt_gret
    .ent psxrt_gret
psxrt_gret:
    SAVE_GUEST
    sw $ra, G(31)($k0)
    TO_HOST
    jal native_gret
    nop
    b restore_and_go
    move $k1, $v0
    .end psxrt_gret

/* GTE_CMD: `sw ra,GPR31(k0); lui k1,hi; jal GTE_CMD; ori k1,k1,lo`. native_gte_fast(k1) is C
 * that keeps every register but at, v0 and a0 (gte_fast.c): those, gp, sp and HI/LO are saved. */
    .globl psxrt_gte_cmd
    .ent psxrt_gte_cmd
psxrt_gte_cmd:
    sw $ra, GTE_RET($k0)
    .irp r,1,2,4,28,29
    sw $\r, G(\r)($k0)
    .endr
    mfhi $at
    sw $at, HI($k0)
    mflo $at
    sw $at, LO($k0)
    move $a0, $k1
    TO_HOST
    jal native_gte_fast
    nop
    lui $k0, %hi(K0_VALUE)
    ori $k0, $k0, %lo(K0_VALUE)
    lw $at, HI($k0)
    mthi $at
    lw $at, LO($k0)
    mtlo $at
    .irp r,1,2,4,28,29,31
    lw $\r, G(\r)($k0)
    .endr
    lw $k1, GTE_RET($k0)
    jr $k1
    nop
    .end psxrt_gte_cmd

/* IO_READ: `sw ra,GPR31(k0); jal IO_READ; addiu k1,rs,off; .word pc; .word insn; j back;
 * lw rt,GPR(rt)(k0)`, the guest address in k1. native_io_read(k1) is C that keeps every register
 * but at, v0 and a0 (io_fast.c): those, gp, sp and HI/LO are saved. It returns 0 with the value
 * in CTX: on past the two words. Otherwise the guest registers are back as they came and the
 * site goes on as a host call, ra the address of its two words. */
    .globl psxrt_io_read
    .ent psxrt_io_read
psxrt_io_read:
    sw $ra, GTE_RET($k0)
    .irp r,1,2,4,28,29
    sw $\r, G(\r)($k0)
    .endr
    mfhi $at
    sw $at, HI($k0)
    mflo $at
    sw $at, LO($k0)
    move $a0, $k1
    TO_HOST
    jal native_io_read
    nop
    lui $k0, %hi(K0_VALUE)
    ori $k0, $k0, %lo(K0_VALUE)
    lw $at, HI($k0)
    mthi $at
    lw $at, LO($k0)
    mtlo $at
    .irp r,1,4,28,29
    lw $\r, G(\r)($k0)
    .endr
    lw $k1, GTE_RET($k0)
    bnez $v0, 1f
    lw $v0, G(2)($k0)
    addiu $k1, $k1, 8
    jr $k1
    lw $ra, G(31)($k0)
1:  b psxrt_rt_call
    move $ra, $k1
    .end psxrt_io_read

/* IO_WRITE: `sw rt,GPR(rt)(k0); sw ra,GPR31(k0); jal IO_WRITE; addiu k1,rs,off; .word pc; .word
 * insn; j back; nop`, the guest address in k1. native_io_write(k1) is ordinary C (native.c), so
 * every caller-saved register is saved; the stored value is in CTX either way. It returns 0 when
 * the write is done: on past the two words. Otherwise the site goes on as a host call. */
    .globl psxrt_io_write
    .ent psxrt_io_write
psxrt_io_write:
    sw $ra, GTE_RET($k0)
    .irp r,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,24,25,28,29
    sw $\r, G(\r)($k0)
    .endr
    mfhi $at
    sw $at, HI($k0)
    mflo $at
    sw $at, LO($k0)
    move $a0, $k1
    TO_HOST
    jal native_io_write
    nop
    lui $k0, %hi(K0_VALUE)
    ori $k0, $k0, %lo(K0_VALUE)
    lw $at, HI($k0)
    mthi $at
    lw $at, LO($k0)
    mtlo $at
    .irp r,1,3,4,5,6,7,8,9,10,11,12,13,14,15,24,25,28,29
    lw $\r, G(\r)($k0)
    .endr
    lw $k1, GTE_RET($k0)
    bnez $v0, 1f
    lw $v0, G(2)($k0)
    addiu $k1, $k1, 8
    jr $k1
    lw $ra, G(31)($k0)
1:  b psxrt_rt_call
    move $ra, $k1
    .end psxrt_io_write

/* GTE_CMD when psxrt.trace bit 7 asks for the GTE's time: native_gte_prof is ordinary C, so
 * every caller-saved register is saved. */
    .globl psxrt_gte_prof
    .ent psxrt_gte_prof
psxrt_gte_prof:
    sw $ra, GTE_RET($k0)
    .irp r,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,24,25,28,29
    sw $\r, G(\r)($k0)
    .endr
    mfhi $at
    sw $at, HI($k0)
    mflo $at
    sw $at, LO($k0)
    move $a0, $k1
    TO_HOST
    jal native_gte_prof
    nop
    lui $k0, %hi(K0_VALUE)
    ori $k0, $k0, %lo(K0_VALUE)
    lw $at, HI($k0)
    mthi $at
    lw $at, LO($k0)
    mtlo $at
    .irp r,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,24,25,28,29,31
    lw $\r, G(\r)($k0)
    .endr
    lw $k1, GTE_RET($k0)
    jr $k1
    nop
    .end psxrt_gte_prof

/* NCLIP, AVSZ3, AVSZ4, RTPS and RTPT without the runtime's C: `sw ra,GPR31(k0); jal GTE_x; nop` from
 * translated code. ra is free (the guest's is in CTX) and k1 keeps the return; t0 and t1 are
 * parked in CTX, HI/LO too since the products use them. Same results as core/gte.c, which
 * native_init checks on random registers before installing them. */
#define GD(r) (0x4B4 + 4 * (r)) /* GTE data register r from k0 */
#define GC(r) (0x534 + 4 * (r)) /* GTE control register r from k0 */

.macro GTE_ENTER
    move $k1, $ra
    mfhi $ra
    sw $ra, HI($k0)
    mflo $ra
    sw $ra, LO($k0)
    sw $8, G(8)($k0)
    sw $9, G(9)($k0)
.endm

    .globl psxrt_gte_nclip
    .ent psxrt_gte_nclip
psxrt_gte_nclip:
    GTE_ENTER
    /* x0*y1 + x1*y2 + x2*y0 - x0*y2 - x1*y0 - x2*y1 */
    lh $8, GD(12)($k0)
    lh $9, GD(13) + 2($k0)
    mult $8, $9
    lh $ra, GD(14) + 2($k0)
    msub $8, $ra
    lh $8, GD(13)($k0)
    madd $8, $ra
    lh $9, GD(12) + 2($k0)
    msub $8, $9
    lh $8, GD(14)($k0)
    madd $8, $9
    lh $9, GD(13) + 2($k0)
    msub $8, $9
    mflo $8
    mfhi $9
    sw $8, GD(24)($k0)
    sra $ra, $8, 31
    bne $ra, $9, mac0_over
    move $ra, $zero /* FLAG */
gte_leave:
    sw $ra, GC(31)($k0)
    lw $8, G(8)($k0)
    lw $9, G(9)($k0)
    lw $ra, HI($k0)
    mthi $ra
    lw $ra, LO($k0)
    mtlo $ra
    jr $k1
    lw $ra, G(31)($k0)
/* MAC0 does not hold HI:LO: FLAG bit 16 (positive) or 15, and 31 */
mac0_over:
    lui $ra, 0x8001
    bgez $9, gte_leave
    nop
    lui $ra, 0x8000
    b gte_leave
    ori $ra, $ra, 0x8000
    .end psxrt_gte_nclip

    .globl psxrt_gte_avsz3
    .ent psxrt_gte_avsz3
psxrt_gte_avsz3:
    GTE_ENTER
    lw $8, GD(17)($k0)
    lw $9, GD(18)($k0)
    addu $8, $8, $9
    lw $9, GD(19)($k0)
    addu $8, $8, $9
    b avsz
    lh $9, GC(29)($k0)
    .end psxrt_gte_avsz3

    .globl psxrt_gte_avsz4
    .ent psxrt_gte_avsz4
psxrt_gte_avsz4:
    GTE_ENTER
    lw $8, GD(16)($k0)
    lw $9, GD(17)($k0)
    addu $8, $8, $9
    lw $9, GD(18)($k0)
    addu $8, $8, $9
    lw $9, GD(19)($k0)
    addu $8, $8, $9
    lh $9, GC(30)($k0)
/* t0 = SZ sum, t1 = ZSF: MAC0 = the product, OTZ = product >> 12 saturated to 0..0xFFFF */
avsz:
    mult $9, $8
    mflo $8
    mfhi $9
    sw $8, GD(24)($k0)
    sra $ra, $8, 31
    bne $ra, $9, avsz_over
    move $ra, $zero /* FLAG */
avsz_otz:
    srl $8, $8, 12
    ins $8, $9, 20, 12
    srl $9, $8, 16
    bnez $9, avsz_sat
    nop
avsz_store:
    b gte_leave
    sw $8, GD(7)($k0)
/* FLAG bit 18, and 31 */
avsz_sat:
    lui $9, 0x8004
    or $ra, $ra, $9
    slt $9, $8, $zero
    ori $8, $zero, 0xFFFF
    b avsz_store
    movn $8, $zero, $9
avsz_over:
    lui $ra, 0x8001
    bgez $9, avsz_otz
    nop
    lui $ra, 0x8000
    b avsz_otz
    ori $ra, $ra, 0x8000
    .end psxrt_gte_avsz4

/* RTPS and RTPT with sf = 1 and lm = 0 (the words the translator sends here), as core/gte.c's
 * rtp_fast: TR must fit 31 bits so the sums cannot leave 44, otherwise GTE_CMD runs them.
 * t2-t7 are parked as well; t4 gathers FLAG and t6 holds the divide's result. */

/* MAC(n+1) of the vertex in t0-t2, IR(n+1) saturated to -0x8000..0x7FFF (FLAG bit 24-n).
 * Leaves the MAC in t3. */
.macro RTP_ROW n
    lw $11, GC(5 + \n)($k0)
    sll $15, $11, 12
    mtlo $15
    sra $15, $11, 20
    mthi $15
    lh $15, GC(0) + 6 * \n($k0)
    madd $15, $8
    lh $15, GC(0) + 6 * \n + 2($k0)
    madd $15, $9
    lh $15, GC(0) + 6 * \n + 4($k0)
    madd $15, $10
    mflo $11
    mfhi $15
    srl $11, $11, 12
    ins $11, $15, 20, 12
    sw $11, GD(25 + \n)($k0)
    seh $15, $11
    beq $15, $11, .Lrow\@
    sw $15, GD(9 + \n)($k0)
    sra $15, $11, 31
    xori $15, $15, 0x7FFF
    sw $15, GD(9 + \n)($k0)
    lui $ra, 1 << (8 - \n)
    or $12, $12, $ra
.Lrow\@:
.endm

/* \out = (q * IR(\ir) + C[\of]) >> 16 with the MAC0 range flags (15/16), saturated to
 * -0x400..0x3FF (FLAG \bit). */
.macro RTP_SXY ir, of, bit, out
    lw $8, GC(\of)($k0)
    sra $9, $8, 31
    mtlo $8
    mthi $9
    lw $9, GD(8 + \ir)($k0)
    madd $14, $9
    mflo $8
    mfhi $9
    sra $15, $8, 31
    beq $15, $9, .Lm\@
    srl \out, $8, 16
    lui $ra, 1
    bgez $9, .Lmf\@
    nop
    ori $ra, $zero, 0x8000
.Lmf\@:
    or $12, $12, $ra
.Lm\@:
    ins \out, $9, 16, 16
    sra $15, \out, 10
    addiu $15, $15, 1
    sltiu $15, $15, 2
    bnez $15, .Ls\@
    sra $15, \out, 31
    xori \out, $15, 0x3FF
    ori $12, $12, \bit
.Ls\@:
.endm

/* Vertex \v: MAC/IR 1-3, SZ (written to data register \sz), q in t6, SXY (to \sxy). */
.macro RTP_V v, sz, sxy
    lh $8, GD(2 * \v)($k0)
    lh $9, GD(2 * \v) + 2($k0)
    lh $10, GD(2 * \v + 1)($k0)
    RTP_ROW 0
    RTP_ROW 1
    RTP_ROW 2
    /* SZ = MAC3 saturated to 0..0xFFFF (FLAG bit 18) */
    srl $15, $11, 16
    beqz $15, .Lsz\@
    move $10, $11
    slt $15, $11, $zero
    ori $10, $zero, 0xFFFF
    movn $10, $zero, $15
    lui $ra, 0x0004
    or $12, $12, $ra
.Lsz\@:
    sw $10, GD(\sz)($k0)
    /* q = H / SZ by the UNR table; 0x1FFFF and FLAG bit 17 when H >= 2 * SZ */
    lhu $8, GC(26)($k0)
    sll $15, $10, 1
    sltu $15, $8, $15
    bnez $15, .Ldiv\@
    lui $14, 1
    ori $14, $14, 0xFFFF
    lui $ra, 0x0002
    b .Lq\@
    or $12, $12, $ra
.Ldiv\@:
    clz $15, $10
    addiu $15, $15, -16
    sllv $8, $8, $15
    sllv $9, $10, $15
    addiu $ra, $9, -0x7FC0
    srl $ra, $ra, 7
    lui $11, %hi(psxrt_unr)
    addu $ra, $ra, $11
    lbu $ra, %lo(psxrt_unr)($ra)
    addiu $ra, $ra, 0x101
    mult $9, $ra
    mflo $9
    lui $11, 0x0200
    ori $11, $11, 0x0080
    subu $9, $11, $9
    srl $9, $9, 8
    mult $9, $ra
    mflo $9
    addiu $9, $9, 0x80
    srl $9, $9, 8
    ori $11, $zero, 0x8000
    mtlo $11
    mthi $zero
    maddu $8, $9
    mflo $14
    mfhi $11
    srl $14, $14, 16
    ins $14, $11, 16, 16
    srl $11, $14, 17
    beqz $11, .Lq\@
    lui $11, 1
    ori $14, $11, 0xFFFF
.Lq\@:
    RTP_SXY 1, 24, 0x4000, $10
    RTP_SXY 2, 25, 0x2000, $11
    ins $10, $11, 16, 16
    sw $10, GD(\sxy)($k0)
.endm

/* Parks t2-t7 and checks TR: bits 31 and 30 of each equal, else on to \far. */
.macro RTP_ENTER far
    GTE_ENTER
    .irp r,10,11,12,13,14,15
    sw $\r, G(\r)($k0)
    .endr
    lw $8, GC(5)($k0)
    sll $9, $8, 1
    xor $8, $8, $9
    lw $9, GC(6)($k0)
    sll $10, $9, 1
    xor $9, $9, $10
    or $8, $8, $9
    lw $9, GC(7)($k0)
    sll $10, $9, 1
    xor $9, $9, $10
    or $8, $8, $9
    bltz $8, \far
    move $12, $zero
.endm

    .globl psxrt_gte_rtps
    .ent psxrt_gte_rtps
psxrt_gte_rtps:
    RTP_ENTER rtps_far
    lw $8, GD(17)($k0)
    sw $8, GD(16)($k0)
    lw $8, GD(18)($k0)
    sw $8, GD(17)($k0)
    lw $8, GD(19)($k0)
    sw $8, GD(18)($k0)
    lw $8, GD(13)($k0)
    sw $8, GD(12)($k0)
    lw $8, GD(14)($k0)
    sw $8, GD(13)($k0)
    RTP_V 0, 19, 14
    b rtp_tail
    nop
rtps_far:
    lui $ra, 0x0008
    b rtp_far
    ori $ra, $ra, 0x0001
    .end psxrt_gte_rtps

    .globl psxrt_gte_rtpt
    .ent psxrt_gte_rtpt
psxrt_gte_rtpt:
    RTP_ENTER rtpt_far
    /* three pushes: SZ0 = the old SZ3, the rest new */
    lw $8, GD(19)($k0)
    sw $8, GD(16)($k0)
    RTP_V 0, 17, 12
    RTP_V 1, 18, 13
    RTP_V 2, 19, 14
/* MAC0 = q * DQA + DQB (flags 15/16), IR0 = MAC0 >> 12 saturated to 0..0x1000 (flag 12), then
 * FLAG bit 31 from the error bits. */
rtp_tail:
    lh $8, GC(27)($k0)
    lw $9, GC(28)($k0)
    sra $10, $9, 31
    mtlo $9
    mthi $10
    madd $14, $8
    mflo $8
    mfhi $9
    sw $8, GD(24)($k0)
    sra $15, $8, 31
    beq $15, $9, 1f
    srl $8, $8, 12
    lui $ra, 1
    bgez $9, 2f
    nop
    ori $ra, $zero, 0x8000
2:  or $12, $12, $ra
1:  ins $8, $9, 20, 12
    sltiu $15, $8, 0x1001
    bnez $15, 3f
    slt $15, $8, $zero
    ori $8, $zero, 0x1000
    movn $8, $zero, $15
    ori $12, $12, 0x1000
3:  sw $8, GD(8)($k0)
    lui $15, 0x7F87
    ori $15, $15, 0xE000
    and $15, $12, $15
    beqz $15, 4f
    lui $15, 0x8000
    or $12, $12, $15
4:  move $ra, $12
    .irp r,10,11,12,13,14,15
    lw $\r, G(\r)($k0)
    .endr
    b gte_leave
    nop
rtpt_far:
    lui $ra, 0x0008
    ori $ra, $ra, 0x0030
/* The command word in ra: everything back as it came, then GTE_CMD with ra and k1 swapped. */
rtp_far:
    lw $8, HI($k0)
    mthi $8
    lw $8, LO($k0)
    mtlo $8
    .irp r,8,9,10,11,12,13,14,15
    lw $\r, G(\r)($k0)
    .endr
    xor $k1, $k1, $ra
    xor $ra, $ra, $k1
    xor $k1, $k1, $ra
    j psxrt_gte_cmd
    nop
    .end psxrt_gte_rtpt


/* MVMVA and GPF take their word in k1, as GTE_CMD does: the return waits in GTE_RET, t0-t7 and
 * HI/LO are parked, t4 gathers FLAG. What they do not take (MVMVA's matrix 3 and far-colour
 * bug, a translation that does not fit 31 bits) goes on to GTE_CMD with everything as it came.
 * The IR registers are 16-bit values, as every way of writing them leaves them, so no product or
 * sum here can leave 44 bits. */
.macro WORD_ENTER
    sw $ra, GTE_RET($k0)
    mfhi $ra
    sw $ra, HI($k0)
    mflo $ra
    sw $ra, LO($k0)
    .irp r,8,9,10,11,12,13,14,15
    sw $\r, G(\r)($k0)
    .endr
    move $12, $zero
.endm

/* The MAC in t0 to IR(n+1), saturated by the word's lm bit (FLAG bit 24-n). */
.macro WORD_IR n
    ext $ra, $k1, 10, 1
    bnez $ra, .Llm\@
    seh $9, $8
    beq $9, $8, .Lok\@
    sra $ra, $8, 31 /* the sign, 0 or -1, for the saturated value */
    b .Lsat\@
    xori $9, $ra, 0x7FFF
.Llm\@:
    srl $9, $8, 15
    beqz $9, .Lok\@
    move $9, $8
    slt $9, $8, $zero
    ori $ra, $zero, 0x7FFF
    movn $ra, $zero, $9
    move $9, $ra
.Lsat\@:
    lui $ra, 1 << (8 - \n)
    or $12, $12, $ra
.Lok\@:
    sw $9, GD(9 + \n)($k0)
.endm

/* HI:LO to MAC(n+1) in t0, shifted by the word's sf bit, then IR(n+1). */
.macro WORD_MAC n
    mflo $8
    ext $ra, $k1, 19, 1
    beqz $ra, .Lsf\@
    mfhi $9
    srl $8, $8, 12
    ins $8, $9, 20, 12
.Lsf\@:
    sw $8, GD(25 + \n)($k0)
    WORD_IR \n
.endm

/* MAC(n+1) >> 4 saturated to 0..0xFF (FLAG bit 21-n) into byte \n of t5. */
.macro WORD_RGB n
    lw $8, GD(25 + \n)($k0)
    sra $8, $8, 4
    srl $9, $8, 8
    beqz $9, .Lc\@
    slt $9, $8, $zero
    ori $8, $zero, 0xFF
    movn $8, $zero, $9
    lui $9, 1 << (5 - \n)
    or $12, $12, $9
.Lc\@:
    ins $13, $8, 8 * \n, 8
.endm

/* MVMVA row n: T[n] * 0x1000 (t2) + row n of the matrix (t3) times the vector (t5-t7). */
.macro MV_ROW n
    lw $8, 4 * \n($10)
    sll $9, $8, 12
    mtlo $9
    sra $9, $8, 20
    mthi $9
    lh $9, GC(0) + 6 * \n($11)
    madd $9, $13
    lh $9, GC(0) + 6 * \n + 2($11)
    madd $9, $14
    lh $9, GC(0) + 6 * \n + 4($11)
    madd $9, $15
    WORD_MAC \n
.endm

    .globl psxrt_gte_mvmva
    .ent psxrt_gte_mvmva
psxrt_gte_mvmva:
    WORD_ENTER
    ext $8, $k1, 17, 2 /* mx: RT, LLM, LCM, 3 the garbage one */
    xori $9, $8, 3
    beqz $9, word_far
    sll $8, $8, 5
    addu $11, $k0, $8
    ext $9, $k1, 13, 2 /* cv: TR, BK, FC (the bug), 3 none */
    xori $10, $9, 3
    beqz $10, mv_none
    xori $10, $9, 2
    beqz $10, word_far
    sll $9, $9, 5
    addu $10, $k0, $9
    addiu $10, $10, GC(5)
    lw $8, 0($10)
    sll $9, $8, 1
    xor $8, $8, $9
    lw $9, 4($10)
    sll $13, $9, 1
    xor $9, $9, $13
    or $8, $8, $9
    lw $9, 8($10)
    sll $13, $9, 1
    xor $9, $9, $13
    or $8, $8, $9
    bltz $8, word_far
    nop
    b mv_vec
    nop
mv_none:
    lui $10, %hi(psxrt_zero3)
    addiu $10, $10, %lo(psxrt_zero3)
mv_vec:
    ext $9, $k1, 15, 2 /* v: V0-V2, 3 IR */
    xori $8, $9, 3
    bnez $8, mv_vxyz
    sll $9, $9, 3
    lw $13, GD(9)($k0)
    lw $14, GD(10)($k0)
    b mv_rows
    lw $15, GD(11)($k0)
mv_vxyz:
    addu $9, $k0, $9
    lh $13, GD(0)($9)
    lh $14, GD(0) + 2($9)
    lh $15, GD(1)($9)
mv_rows:
    MV_ROW 0
    MV_ROW 1
    MV_ROW 2
    b word_leave
    nop
    .end psxrt_gte_mvmva

/* GPF: MAC = IR0 * IR, then the colour pushed. */
.macro GPF_ROW n
    lw $9, GD(9 + \n)($k0)
    mult $10, $9
    WORD_MAC \n
.endm

    .globl psxrt_gte_gpf
    .ent psxrt_gte_gpf
psxrt_gte_gpf:
    WORD_ENTER
    lw $10, GD(8)($k0)
    GPF_ROW 0
    GPF_ROW 1
    GPF_ROW 2
    lw $8, GD(21)($k0)
    sw $8, GD(20)($k0)
    lw $8, GD(22)($k0)
    sw $8, GD(21)($k0)
    lw $13, GD(6)($k0)
    srl $13, $13, 24
    sll $13, $13, 24
    WORD_RGB 0
    WORD_RGB 1
    WORD_RGB 2
    sw $13, GD(22)($k0)
/* FLAG with bit 31 from its error bits, the parked registers back, return through GTE_RET. */
word_leave:
    lui $8, 0x7F87
    ori $8, $8, 0xE000
    and $8, $12, $8
    beqz $8, 1f
    lui $8, 0x8000
    or $12, $12, $8
1:  move $ra, $12
    .irp r,10,11,12,13,14,15
    lw $\r, G(\r)($k0)
    .endr
    b gte_leave
    lw $k1, GTE_RET($k0)
word_far:
    lw $8, HI($k0)
    mthi $8
    lw $8, LO($k0)
    mtlo $8
    .irp r,8,9,10,11,12,13,14,15
    lw $\r, G(\r)($k0)
    .endr
    j psxrt_gte_cmd
    lw $ra, GTE_RET($k0)
    .end psxrt_gte_gpf

/* DPCS, colour component n (core/gte.c interpolate): in = RGBC byte n << 16 (t6, t7 = 1);
 * MAC = FC * 0x1000 - in, shifted by sf, to IR saturated with lm 0 (FLAG bit 24-n) in t1; then
 * MAC = IR * IR0 (t2) + in, MAC and IR by the word's sf and lm. An FC within 31 bits keeps the
 * first sum within 43 bits, so it never overflows; the second fits 31 bits. */
.macro DP_ROW n
    ext $14, $11, 8 * \n, 8
    sll $14, $14, 16
    lw $13, GC(21 + \n)($k0)
    sll $9, $13, 12
    mtlo $9
    sra $9, $13, 20
    mthi $9
    msub $14, $15
    mflo $8
    ext $ra, $k1, 19, 1
    beqz $ra, .Lsf\@
    mfhi $9
    srl $8, $8, 12
    ins $8, $9, 20, 12
.Lsf\@:
    seh $9, $8
    beq $9, $8, .Lok\@
    sra $ra, $8, 31
    xori $9, $ra, 0x7FFF
    lui $ra, 1 << (8 - \n)
    or $12, $12, $ra
.Lok\@:
    mtlo $14
    mthi $zero
    madd $9, $10
    WORD_MAC \n
.endm

/* DPCS: the depth cue of RGBC towards FC by IR0, then the colour pushed. An FC beyond 31 bits
 * goes to GTE_CMD. */
    .globl psxrt_gte_dpcs
    .ent psxrt_gte_dpcs
psxrt_gte_dpcs:
    WORD_ENTER
    lw $8, GC(21)($k0)
    sll $9, $8, 1
    xor $8, $8, $9
    lw $9, GC(22)($k0)
    sll $13, $9, 1
    xor $9, $9, $13
    or $8, $8, $9
    lw $9, GC(23)($k0)
    sll $13, $9, 1
    xor $9, $9, $13
    or $8, $8, $9
    bltz $8, word_far
    lw $11, GD(6)($k0)
    lw $10, GD(8)($k0)
    li $15, 1
    DP_ROW 0
    DP_ROW 1
    DP_ROW 2
    lw $8, GD(21)($k0)
    sw $8, GD(20)($k0)
    lw $8, GD(22)($k0)
    sw $8, GD(21)($k0)
    srl $13, $11, 24
    sll $13, $13, 24
    WORD_RGB 0
    WORD_RGB 1
    WORD_RGB 2
    b word_charge
    sw $13, GD(22)($k0)
    .end psxrt_gte_dpcs

/* SQR: MAC = IR * IR, by the word's sf and lm. */
.macro SQR_ROW n
    lw $9, GD(9 + \n)($k0)
    mult $9, $9
    WORD_MAC \n
.endm

    .globl psxrt_gte_sqr
    .ent psxrt_gte_sqr
psxrt_gte_sqr:
    WORD_ENTER
    SQR_ROW 0
    SQR_ROW 1
    SQR_ROW 2
/* The 10 cycles GTE_CMD adds (gte_fast.c), taken from the budget: PS1 time as through GTE_CMD. */
word_charge:
    lw $8, BUDGET($k0)
    addiu $8, $8, -10
    b word_leave
    sw $8, BUDGET($k0)
    .end psxrt_gte_sqr
/* The same entries through GTE_CMD, for when native_init's check finds a difference. */
.macro GTE_DOOR name, cmd
    .globl psxrt_gte_\name\()_door
    .ent psxrt_gte_\name\()_door
psxrt_gte_\name\()_door:
    lui $k1, \cmd >> 16
    j psxrt_gte_cmd
    ori $k1, $k1, \cmd & 0xFFFF
    .end psxrt_gte_\name\()_door
.endm
    GTE_DOOR nclip, 0x00000006
    GTE_DOOR avsz3, 0x0000002D
    GTE_DOOR avsz4, 0x0000002E
    GTE_DOOR rtps, 0x00080001
    GTE_DOOR rtpt, 0x00080030

/* The guest state from CTX, then on to the host address in k1. */
    .ent restore_and_go
restore_and_go:
    lui $k0, %hi(K0_VALUE)
    ori $k0, $k0, %lo(K0_VALUE)
    lw $at, HI($k0)
    mthi $at
    lw $at, LO($k0)
    mtlo $at
    .irp r,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,28,29,30,31
    lw $\r, G(\r)($k0)
    .endr
    jr $k1
    nop
    .end restore_and_go

/* void native_enter(u32 host_pc): runs translated code at host_pc with the guest state of CTX
 * until it reaches CALL_RET; the guest state is then in CTX. Nests. */
    .globl native_enter
    .ent native_enter
native_enter:
    addiu $sp, $sp, -FRAME
    sw $s0, F_S0 + 0($sp)
    sw $s1, F_S0 + 4($sp)
    sw $s2, F_S0 + 8($sp)
    sw $s3, F_S0 + 12($sp)
    sw $s4, F_S0 + 16($sp)
    sw $s5, F_S0 + 20($sp)
    sw $s6, F_S0 + 24($sp)
    sw $s7, F_S0 + 28($sp)
    sw $fp, F_FP($sp)
    sw $ra, F_RA($sp)
    sw $gp, F_GP($sp)
    lui $t0, %hi(native_host_sp)
    lw $t1, %lo(native_host_sp)($t0)
    sw $t1, F_PREV($sp)
    sw $sp, %lo(native_host_sp)($t0)
    b restore_and_go
    move $k1, $a0
    .end native_enter

    .globl psxrt_call_ret
    .ent psxrt_call_ret
psxrt_call_ret:
    SAVE_GUEST
    sw $ra, G(31)($k0)
    lui $t0, %hi(native_host_sp)
    lw $sp, %lo(native_host_sp)($t0)
    lw $t1, F_PREV($sp)
    sw $t1, %lo(native_host_sp)($t0)
    lw $s0, F_S0 + 0($sp)
    lw $s1, F_S0 + 4($sp)
    lw $s2, F_S0 + 8($sp)
    lw $s3, F_S0 + 12($sp)
    lw $s4, F_S0 + 16($sp)
    lw $s5, F_S0 + 20($sp)
    lw $s6, F_S0 + 24($sp)
    lw $s7, F_S0 + 28($sp)
    lw $fp, F_FP($sp)
    lw $ra, F_RA($sp)
    lw $gp, F_GP($sp)
    move $k1, $zero
    jr $ra
    addiu $sp, $sp, FRAME
    .end psxrt_call_ret

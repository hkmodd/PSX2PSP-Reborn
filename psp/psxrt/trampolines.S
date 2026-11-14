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
#define GTE_RET 0x448 /* CTX.GTE_RET */

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

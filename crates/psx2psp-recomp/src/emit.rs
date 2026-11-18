//! Allegrex emitter: the discovered code of one executable translated into PSP machine code.
//!
//! Guest registers are host registers (identity), except `k0`/`k1`: see [`crate::layout`]. Most
//! instructions are copied as they are; the rest expand:
//!
//! * `add`/`addi`/`sub` become `addu`/`addiu`/`subu` (games do not rely on the overflow trap).
//! * `div`/`divu` get explicit guards: the R3000A's results for a zero divisor and for
//!   `i32::MIN / -1` are defined, the Allegrex's are not.
//! * Loads and stores compute the guest address in `k1`. RAM (bit 23 clear) maps with one
//!   `ins`; anything else takes an out-of-line path: the scratchpad is addressed from `k0`, and
//!   I/O, BIOS and unmapped addresses become a host call that runs the access in the runtime.
//!   Loads and stores that share a base with only plain words between them are one group: its
//!   lowest and highest address are checked once (RAM, one 2 MB mirror) and each access is then
//!   one host word; a failed check runs an out-of-line copy of the group, access by access.
//!   Accesses are 70% of the main-pass code, which the I-cache refills (LEV01: -14% of it).
//! * Calls are host calls (`jal` writes a host `ra`, a leaf's `jr ra` returns natively). A `jal`
//!   outside the image, `jr` and `jalr`, and the `jr ra` of a function that writes `ra`, go
//!   through the dispatch table ([`leaf_returns`]).
//! * GTE moves, `lwc2` and `swc2` work directly on the GTE registers at [`layout::GTE`] (but
//!   for the few registers with unusual rules); GTE commands enter [`layout::GTE_CMD`], or the
//!   entry of their own [`layout::GTE_OWN`] or [`layout::GTE_WORD`] gives them.
//! * COP0, the rest of the GTE, `syscall`, `break` and anything touching the guest's `k0`/`k1`
//!   run in the runtime through a host call and continue after it.
//! * Anything the translator does not model hands the guest over to the runtime's interpreter
//!   ("resume"), which comes back to translated code through the dispatch table.
//! * Loop back-edges charge the loop's estimated PS1 cycles to [`ctx::BUDGET`] first and enter
//!   the runtime when it goes negative: the runtime advances the PS1 clock by what was spent
//!   and serves interrupts (it also forces the budget negative to deliver one).
//!
//! Host call, five words: `sw ra,GPR31(k0); jal RT_CALL; nop; .word pc; .word insn`. The runtime
//! saves the guest state to [`CTX`], services pending interrupts, executes `insn` as the guest
//! instruction at `pc`, restores the state and returns past the two data words. With
//! [`RESUME`] set in `pc` it interprets from `pc` instead and never returns there.
//!
//! Delay slots: a slot that is one plain host instruction stays in the host slot. Otherwise the
//! slot is moved before the transfer when it does not write a register the transfer reads, or
//! duplicated on both paths of a conditional branch, or (register jumps) run between saving and
//! reloading the host target.

use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;

use crate::discover::{Discovery, Image, JumpReg};
use crate::exe::PsxExe;
use crate::hazard::Hazards;
use crate::hooks::{self, HOOK, Hook};
use crate::insn::{Insn, Isa, Kind, Uses, decode};
use crate::layout::{self, RT_CALL, SCRATCH, ctx, gte, k0_off};

/// Flag in a host call's `pc` word: interpret from `pc`, do not return.
pub const RESUME: u32 = 1;

const ZR: u32 = 0;
const K0R: u32 = 26;
const K1R: u32 = 27;
const RA: u32 = 31;
const GUEST_K: u32 = (1 << K0R) | (1 << K1R);
const NOP: u32 = 0;
/// Upper bound of a loop's charge ([`Em::loop_cost`]).
const MAX_LOOP_COST: u32 = 1024;
/// Cycles a read from PS1 RAM adds to its instruction: the bus access (DuckStation's
/// `RAM_READ_TICKS`). Used by [`Em::loop_cost`].
const RAM_READ: u32 = 6;
/// Most guest words in one group of loads and stores ([`Em::run_from`]).
const GROUP_MAX: usize = 64;

/// Why the runtime handles a guest instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Reason {
    /// Executed by the runtime, then translated code continues.
    Cop0,
    Cop2,
    Syscall,
    Break,
    /// Reads or writes the guest's `k0`/`k1`.
    GuestK,
    /// From here on the runtime's interpreter runs the guest.
    Invalid,
    /// The path leaves the discovered code.
    Unreached,
    /// A branch or jump to an address outside the executable.
    Outside,
    /// A transfer in a delay slot.
    BranchInSlot,
    /// The delay slot uses the link register its call writes.
    LinkInSlot,
    /// The delay slot of a linking branch writes a register the branch reads.
    SlotClobbers,
    /// A load whose delay slot the code depends on (see [`crate::hazard`]).
    Hazard,
    /// A recipe the runtime runs itself ([`crate::hooks`]).
    Hook,
}

impl Reason {
    /// True if the interpreter takes over instead of the code continuing.
    pub const fn resumes(self) -> bool {
        !matches!(self, Reason::Cop0 | Reason::Cop2 | Reason::Syscall | Reason::Break | Reason::GuestK | Reason::Hook)
    }
}

/// Translated code of one executable.
#[derive(Clone, Debug, Default)]
pub struct Translation {
    /// Host address of `words[0]`.
    pub base: u32,
    pub words: Vec<u32>,
    /// Guest instruction -> host address of its translation. Every discovered instruction except
    /// delay slots that nothing jumps to; this is the content of the dispatch table.
    pub map: BTreeMap<u32, u32>,
    /// Guest instructions handled by the runtime, and why.
    pub runtime: BTreeMap<u32, Reason>,
    /// Guest instruction -> host addresses of the words that copy it ([`simple`] ones only: one
    /// address, or two for a delay slot duplicated on both paths or also jumped to). When the
    /// game rewrites such an instruction, [`repatch`] gives the new host word for each address.
    /// Also the words of a load or store's translation that hold its offset ([`takes_offset`]).
    pub patch: BTreeMap<u32, Vec<u32>>,
    /// Words of out-of-line code (slow paths, interrupt checks, host calls, long jumps), and the
    /// number of blocks they are written in between the main-pass code.
    pub cold: (usize, usize),
    /// Main-pass code in guest order: guest pc, host start, host end of the code a straight-line
    /// run through it executes (a group's span covers its guest words; out-of-line code is not in
    /// it). The I-cache model's map (`examples/icache.rs`).
    pub spans: Vec<(u32, u32, u32)>,
    /// The words that depend on where the code is: host address of the word, and what it holds.
    /// [`Translation::moved`] applies them.
    pub relocs: Vec<(u32, Reloc)>,
    /// [`translate_floating`]: host address of the `lui`/`ori` pair that holds the target of the
    /// `j`/`jal` at a guest pc. A game that links code where it loads it rewrites those targets
    /// (Spider-Man's CD.WAD modules); the runtime writes the pair from the word RAM holds.
    pub targets: Vec<(u32, u32)>,
}

/// What a word of a [`Translation`] holds that changes with the guest or host address of the code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reloc {
    /// A guest address (a host call's `.word pc`): plus the guest delta.
    Guest,
    /// A guest address in the `lui`/`ori` pair that starts at the word (a link a+8 written to a
    /// register other than ra, the target of a `bal` outside the code): plus the guest delta.
    GuestPair,
    /// A `j`/`jal` to host code of the translation: its target plus the host delta.
    Jump,
    /// A host address of the translation in the `lui`/`ori` pair that starts at the word: plus
    /// the host delta.
    HostPair,
}

/// The host word for `guest` written where the simple instruction translated to `host` was, if
/// it is the same instruction with another immediate or shift amount (Sheep Dog's level
/// constants): same opcode and registers, so the code around it stays valid. `None`: the code
/// must be translated again or interpreted.
pub fn repatch(host: u32, guest: u32) -> Option<u32> {
    let op = guest >> 26;
    let new = match op {
        8 => guest | 1 << 26, // addi -> addiu, as `simple` does
        _ => guest,
    };
    let keep = match op {
        8..=0xF => 0xFFFF_0000,                                     // immediates: op, rs, rt
        0 if matches!(guest & 0x3F, 0 | 2 | 3) => !(0x1F << 6), // sll/srl/sra: all but sa
        _ => !0,
    };
    (new & keep == host & keep).then_some(new)
}

/// A load or store, `lwc2`/`swc2` included.
pub fn mem_op(w: u32) -> bool {
    matches!(w >> 26, 0x20..=0x2E | 0x32 | 0x3A)
}

/// Whether the host word `host`, which [`Translation::patch`] lists for the load or store
/// `guest`, holds its offset as it is: the `addiu k1, base, offset` of its address, or the word
/// itself for the runtime. A rewrite of the offset alone is then the host word's low half
/// (`psp/psxrt/floating.c` `fl_place`). A group's own word (an offset from the group's lowest)
/// does not: a group's offsets stay as they are.
pub fn takes_offset(host: u32, guest: u32) -> bool {
    let addr = 9 << 26 | (guest >> 21 & 0x1F) << 21 | K1R << 16;
    mem_op(guest) && host & 0xFFFF == guest & 0xFFFF && (host & 0xFFFF_0000 == addr || host == guest)
}

/// Whether [`repatch`] can take a rewrite of `w` at all: an I-type ALU immediate (`addi` to
/// `lui`) or a constant shift.
pub fn patchable(w: u32) -> bool {
    let op = w >> 26;
    (8..=0xF).contains(&op) || (op == 0 && matches!(w & 0x3F, 0 | 2 | 3))
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum EmitError {
    #[error("guest {pc:#010x}: host branch needs {words} words, beyond the 16-bit range")]
    Range { pc: u32, words: i64 },
    #[error("guest {pc:#010x} transfers to {target:#010x}, which has no translation")]
    Target { pc: u32, target: u32 },
    #[error("code at {0:#010x} must be in the same 256 MB region as the runtime")]
    Base(u32),
}

/// Allegrex encodings used by the emitter.
mod enc {
    pub const fn r(rs: u32, rt: u32, rd: u32, sa: u32, funct: u32) -> u32 {
        (rs << 21) | (rt << 16) | (rd << 11) | (sa << 6) | funct
    }
    pub const fn i(op: u32, rs: u32, rt: u32, imm: u16) -> u32 {
        (op << 26) | (rs << 21) | (rt << 16) | imm as u32
    }
    pub const fn sll(rd: u32, rt: u32, sa: u32) -> u32 {
        r(0, rt, rd, sa, 0)
    }
    pub const fn srl(rd: u32, rt: u32, sa: u32) -> u32 {
        r(0, rt, rd, sa, 2)
    }
    pub const fn jr(rs: u32) -> u32 {
        r(rs, 0, 0, 0, 8)
    }
    pub const fn jalr(rd: u32, rs: u32) -> u32 {
        r(rs, 0, rd, 0, 9)
    }
    pub const fn mthi(rs: u32) -> u32 {
        r(rs, 0, 0, 0, 0x11)
    }
    pub const fn mtlo(rs: u32) -> u32 {
        r(rs, 0, 0, 0, 0x13)
    }
    pub const fn addu(rd: u32, rs: u32, rt: u32) -> u32 {
        r(rs, rt, rd, 0, 0x21)
    }
    pub const fn subu(rd: u32, rs: u32, rt: u32) -> u32 {
        r(rs, rt, rd, 0, 0x23)
    }
    pub const fn addiu(rt: u32, rs: u32, imm: u16) -> u32 {
        i(9, rs, rt, imm)
    }
    pub const fn sltiu(rt: u32, rs: u32, imm: u16) -> u32 {
        i(0x0B, rs, rt, imm)
    }
    pub const fn andi(rt: u32, rs: u32, imm: u16) -> u32 {
        i(0xC, rs, rt, imm)
    }
    pub const fn ori(rt: u32, rs: u32, imm: u16) -> u32 {
        i(0xD, rs, rt, imm)
    }
    pub const fn xori(rt: u32, rs: u32, imm: u16) -> u32 {
        i(0xE, rs, rt, imm)
    }
    pub const fn lui(rt: u32, imm: u16) -> u32 {
        i(0xF, 0, rt, imm)
    }
    pub const fn lh(rt: u32, off: u16, base: u32) -> u32 {
        i(0x21, base, rt, off)
    }
    pub const fn lw(rt: u32, off: u16, base: u32) -> u32 {
        i(0x23, base, rt, off)
    }
    pub const fn lhu(rt: u32, off: u16, base: u32) -> u32 {
        i(0x25, base, rt, off)
    }
    pub const fn sw(rt: u32, off: u16, base: u32) -> u32 {
        i(0x2B, base, rt, off)
    }
    /// Branches without their offset (filled by a fixup).
    pub const fn beq(rs: u32, rt: u32) -> u32 {
        i(4, rs, rt, 0)
    }
    pub const fn bne(rs: u32, rt: u32) -> u32 {
        i(5, rs, rt, 0)
    }
    pub const fn bltz(rs: u32) -> u32 {
        i(1, rs, 0, 0)
    }
    pub const fn jal(addr: u32) -> u32 {
        (3 << 26) | ((addr >> 2) & 0x03FF_FFFF)
    }
    /// `rt` <- `size` bits of `rs` from bit `pos`.
    pub const fn ext(rt: u32, rs: u32, pos: u32, size: u32) -> u32 {
        (0x1F << 26) | (rs << 21) | (rt << 16) | ((size - 1) << 11) | (pos << 6)
    }
    /// `rd` <- low halfword of `rt`, sign-extended.
    pub const fn seh(rd: u32, rt: u32) -> u32 {
        (0x1F << 26) | (rt << 16) | (rd << 11) | (0x18 << 6) | 0x20
    }
    /// Bits `pos .. pos + size` of `rt` <- low bits of `rs`.
    pub const fn ins(rt: u32, rs: u32, pos: u32, size: u32) -> u32 {
        (0x1F << 26) | (rs << 21) | (rt << 16) | ((pos + size - 1) << 11) | (pos << 6) | 4
    }
}

use enc::*;

/// The host word for a guest instruction that translates to exactly itself (or its non-trapping
/// twin) and does not touch the guest's `k0`/`k1`.
fn simple(i: Insn, k: Kind) -> Option<u32> {
    use Kind::*;
    let u = Uses::of(i, k);
    if (u.read | u.write) & GUEST_K != 0 {
        return None;
    }
    match k {
        Sll | Srl | Sra | Sllv | Srlv | Srav | Mfhi | Mthi | Mflo | Mtlo | Mult | Multu | Addu
        | Subu | And | Or | Xor | Nor | Slt | Sltu | Addiu | Slti | Sltiu | Andi | Ori | Xori
        | Lui => Some(i.0),
        Add => Some((i.0 & !0x3F) | 0x21),
        Sub => Some((i.0 & !0x3F) | 0x23),
        Addi => Some((i.0 & 0x03FF_FFFF) | (9 << 26)),
        _ => None,
    }
}

#[derive(Clone, Copy)]
enum To {
    Guest(u32),
    Label(usize),
}

#[derive(Clone, Copy)]
enum Fx {
    /// 16-bit branch offset.
    Branch,
    /// 26-bit `j`/`jal` target.
    Jump,
    /// Host address in a `lui`/`ori` pair.
    Abs,
}

/// A piece of host code with unresolved branches.
#[derive(Default)]
struct Code {
    w: Vec<u32>,
    /// Word index, kind, destination, guest pc for errors.
    fix: Vec<(usize, Fx, To, u32)>,
    /// Label -> word index.
    defs: Vec<(usize, usize)>,
    /// Word index -> guest pc of the simple instruction it copies.
    pat: Vec<(usize, u32)>,
    /// Word index -> the guest address it holds ([`Reloc::Guest`], [`Reloc::GuestPair`]).
    rel: Vec<(usize, Reloc)>,
    /// Word index -> guest pc of the `j`/`jal` whose target the pair there holds.
    tgt: Vec<(usize, u32)>,
}

impl Code {
    fn push(&mut self, w: u32) {
        self.w.push(w);
    }
    /// The host word `w` that copies the simple guest instruction at `pc`.
    fn simple(&mut self, w: u32, pc: u32) {
        self.pat.push((self.w.len(), pc));
        self.w.push(w);
    }
    fn branch(&mut self, w: u32, to: To, pc: u32) {
        self.fix.push((self.w.len(), Fx::Branch, to, pc));
        self.w.push(w & 0xFFFF_0000);
    }
    /// `j` (op 2) or `jal` (op 3).
    fn jump(&mut self, op: u32, to: To, pc: u32) {
        self.fix.push((self.w.len(), Fx::Jump, to, pc));
        self.w.push(op << 26);
    }
    /// `reg` = the host address of `to`.
    fn abs(&mut self, reg: u32, to: To, pc: u32) {
        self.fix.push((self.w.len(), Fx::Abs, to, pc));
        self.w.push(lui(reg, 0));
        self.w.push(ori(reg, reg, 0));
    }
    fn def(&mut self, label: usize) {
        self.defs.push((label, self.w.len()));
    }
    /// `.word a`, a guest address.
    fn guest(&mut self, a: u32) {
        self.rel.push((self.w.len(), Reloc::Guest));
        self.w.push(a);
    }
    /// `reg` = the guest address `a`, which moves with the code when `moves`.
    fn guest_pair(&mut self, reg: u32, a: u32, moves: bool) {
        if moves {
            self.rel.push((self.w.len(), Reloc::GuestPair));
        }
        self.w.push(lui(reg, (a >> 16) as u16));
        self.w.push(ori(reg, reg, a as u16));
    }
    /// `reg` = `t`, the target of the `j`/`jal` at `pc` ([`Translation::targets`]).
    fn target_pair(&mut self, reg: u32, t: u32, pc: u32) {
        self.tgt.push((self.w.len(), pc));
        self.guest_pair(reg, t, false);
    }
    fn append(&mut self, o: Code) {
        let b = self.w.len();
        self.w.extend(o.w);
        self.fix.extend(o.fix.into_iter().map(|(a, f, t, p)| (a + b, f, t, p)));
        self.defs.extend(o.defs.into_iter().map(|(l, a)| (l, a + b)));
        self.pat.extend(o.pat.into_iter().map(|(a, p)| (a + b, p)));
        self.rel.extend(o.rel.into_iter().map(|(a, r)| (a + b, r)));
        self.tgt.extend(o.tgt.into_iter().map(|(a, p)| (a + b, p)));
    }
    /// Host call for the guest instruction `word` at `pc` (`pc | RESUME` to hand over).
    fn host_call(&mut self, pc: u32, word: u32) {
        self.push(sw(RA, k0_off(layout::CTX + ctx::GPR + 4 * RA), K0R));
        self.push(jal(RT_CALL));
        self.push(NOP);
        self.guest(pc);
        self.push(word);
    }
    /// [`Code::host_call`] for the load or store `word` at `pc`, the word listed as a copy of it.
    fn host_call_mem(&mut self, pc: u32, word: u32) {
        self.push(sw(RA, k0_off(layout::CTX + ctx::GPR + 4 * RA), K0R));
        self.push(jal(RT_CALL));
        self.push(NOP);
        self.guest(pc);
        self.simple(word, pc);
    }
}

/// Stubs are cold: a block of them goes after an unconditional transfer once the code since the
/// oldest pending one reaches [`COLD_BLOCK`], so the main-pass code stays dense in the I-cache.
/// They are jumped over where they are once the oldest could fall out of branch range.
const FLUSH_WORDS: usize = 0x4000;
const COLD_BLOCK: usize = FLUSH_WORDS / 2;

struct Em<'a> {
    exe: &'a PsxExe,
    d: &'a Discovery,
    hz: &'a Hazards,
    out: Code,
    stubs: Vec<Code>,
    stub_words: usize,
    oldest: usize,
    labels: usize,
    map: BTreeMap<u32, usize>,
    /// Guest addresses whose translation is at a label (inside a [`Em::group`]'s out-of-line copy).
    map_at: Vec<(u32, usize)>,
    /// Every address something can transfer to directly ([`Em::targets`]).
    targets: BTreeSet<u32>,
    /// The main pass resumes here: the words before it were translated with a group.
    skip_to: u32,
    runtime: BTreeMap<u32, Reason>,
    /// Out-of-line words written so far, and in how many blocks.
    cold: (usize, usize),
    /// [`Translation::spans`] as word indices.
    spans: Vec<(u32, usize, usize)>,
    /// Conditional branches whose guest target is beyond 16 bits in host code.
    long: &'a BTreeSet<u32>,
    hooks: &'a BTreeMap<u32, Hook>,
    /// [`leaf_returns`].
    leaf: &'a BTreeSet<u32>,
    /// [`translate_floating`]: absolute targets are never in the code.
    floating: bool,
}

enum Fail {
    /// Conditional branches (guest pc, host distance) to guest targets out of range.
    Far(Vec<(u32, i64)>),
    Error(EmitError),
}

/// Translates the discovered code of `exe` to host code at `base`.
///
/// A conditional branch takes its guest target directly when it is within 16 bits in host code;
/// the others go through a nearby `j`. Which ones need it is known only once the code is laid
/// out, so the translation repeats until every branch fits.
pub fn translate(
    exe: &PsxExe,
    d: &Discovery,
    hz: &Hazards,
    base: u32,
) -> Result<Translation, EmitError> {
    translate_with(exe, d, hz, base, &hooks::find(exe, d))
}

/// [`translate`] with the hooks given ([`hooks::find`] of another text: the runtime keeps the
/// hooks of the text it translated when the game rewrites words of it).
pub fn translate_with(
    exe: &PsxExe,
    d: &Discovery,
    hz: &Hazards,
    base: u32,
    hooks: &BTreeMap<u32, Hook>,
) -> Result<Translation, EmitError> {
    translate_as(exe, d, hz, base, hooks, false)
}

/// [`translate`] of code that runs wherever the game puts it (Crash 2's native GOOL code is
/// position-independent): `j`, `jal` and the `lui` constants are never taken as addresses in it,
/// and no hooks. [`Translation::moved`] places it at another guest and host address.
pub fn translate_floating(exe: &PsxExe, d: &Discovery, hz: &Hazards, base: u32) -> Result<Translation, EmitError> {
    translate_as(exe, d, hz, base, &BTreeMap::new(), true)
}

fn translate_as(
    exe: &PsxExe,
    d: &Discovery,
    hz: &Hazards,
    base: u32,
    hooks: &BTreeMap<u32, Hook>,
    floating: bool,
) -> Result<Translation, EmitError> {
    if base & 3 != 0 || (base ^ RT_CALL) & 0xF000_0000 != 0 {
        return Err(EmitError::Base(base));
    }
    let mut long = BTreeSet::new();
    let leaf = leaf_returns(exe, d);
    loop {
        match emit(exe, d, hz, base, &long, hooks, &leaf, floating) {
            Ok(t) => return Ok(t),
            Err(Fail::Error(e)) => return Err(e),
            Err(Fail::Far(far)) => {
                let n = long.len();
                long.extend(far.iter().map(|&(pc, _)| pc));
                if long.len() == n {
                    let (pc, words) = far[0];
                    return Err(EmitError::Range { pc, words });
                }
            }
        }
    }
}

/// The `jr ra` sites that may return natively: every function body they end writes no `ra`, so
/// `ra` is the caller's, which a translated call or the runtime's hand-over (`fix_ra`) made host
/// code. Any other `jr ra` may read an `ra` from memory that an interpreted prologue saved as a
/// guest address: Silent Hill's switch case at 80071C08 (a table outside the image, so its
/// bodies are interpreted) calls 80073E3C with the guest ra 80071C10, and a translated
/// `lw ra; jr ra` jumped to PSP 0x80071C10 at f1808. Those go through [`Em::dispatch`]: host
/// code as it is, a guest address through the table.
fn leaf_returns(exe: &PsxExe, d: &Discovery) -> BTreeSet<u32> {
    use Kind::*;
    let img = Image(exe, &[], false);
    let writes_ra = |a: u32| {
        img.at(a).is_some_and(|(i, k)| {
            Uses::of(i, k).write & (1 << RA) != 0
                || matches!(k, Jal | Bltzal | Bgezal)
                || (k == Jalr && i.rd() == RA as usize)
        })
    };
    let (mut leaf, mut not) = (BTreeSet::new(), BTreeSet::new());
    for &e in d.entries.keys() {
        let b = d.body(exe, e);
        let into = if b.insns.iter().any(|&a| writes_ra(a)) { &mut not } else { &mut leaf };
        into.extend(b.returns);
    }
    &leaf - &not
}

fn emit(
    exe: &PsxExe,
    d: &Discovery,
    hz: &Hazards,
    base: u32,
    long: &BTreeSet<u32>,
    hooks: &BTreeMap<u32, Hook>,
    leaf: &BTreeSet<u32>,
    floating: bool,
) -> Result<Translation, Fail> {
    let mut em = Em {
        exe,
        d,
        hz,
        long,
        hooks,
        leaf,
        floating,
        out: Code::default(),
        stubs: Vec::new(),
        stub_words: 0,
        oldest: 0,
        labels: 0,
        map: BTreeMap::new(),
        map_at: Vec::new(),
        targets: BTreeSet::new(),
        skip_to: 0,
        runtime: BTreeMap::new(),
        cold: (0, 0),
        spans: Vec::new(),
    };
    em.targets = em.targets();
    let targets = em.targets.clone();
    for &a in d.code.iter().filter(|a| !d.delay_slots.contains_key(a)) {
        if a < em.skip_to {
            continue;
        }
        em.far_flush();
        let start = em.out.w.len();
        em.map.insert(a, start);
        let next = em.insn(a);
        if let Some(n) = next.filter(|&n| !em.first(n)) {
            let c = em.resume(n, Reason::Unreached);
            em.out.append(c);
        }
        em.spans.push((a, start, em.out.w.len()));
        if !next.is_some_and(|n| em.first(n)) {
            em.run_end();
        }
    }
    // Delay slots that are also jumped to get a copy of their own.
    for &s in d.delay_slots.keys().filter(|s| targets.contains(s) && d.code.contains(s)) {
        em.far_flush();
        em.map.insert(s, em.out.w.len());
        let c = match em.at(s) {
            Some((i, k)) if !k.has_delay_slot() && !em.hz.sites.contains_key(&s) => {
                let (mut c, falls) = em.plain(s, i, k);
                if falls {
                    if em.first(s + 4) {
                        c.jump(2, To::Guest(s + 4), s);
                        c.push(NOP);
                    } else {
                        c.append(em.resume(s + 4, Reason::Unreached));
                    }
                }
                c
            }
            _ => em.resume(s, Reason::Invalid),
        };
        em.out.append(c);
        em.run_end();
    }
    em.flush();
    em.finish(base)
}

impl Em<'_> {
    fn at(&self, a: u32) -> Option<(Insn, Kind)> {
        let i = Insn(self.exe.word(a)?);
        Some((i, decode(i, Isa::R3000A)))
    }

    /// True if `a` is translated in the main pass (code, not a delay slot).
    fn first(&self, a: u32) -> bool {
        self.d.code.contains(&a) && !self.d.delay_slots.contains_key(&a)
    }

    fn label(&mut self) -> usize {
        self.labels += 1;
        self.labels - 1
    }

    /// Every address something can transfer to directly.
    fn targets(&self) -> BTreeSet<u32> {
        use Kind::*;
        let d = self.d;
        let mut t: BTreeSet<u32> = d.entries.keys().copied().collect();
        t.extend(d.calls.keys());
        t.extend(d.indirect_calls.values().flatten());
        for j in d.jumps.values() {
            match j {
                JumpReg::Table { targets, .. } => t.extend(targets),
                JumpReg::Const(c) => {
                    t.insert(*c);
                }
                JumpReg::Unresolved(_) => {}
            }
        }
        for &a in &d.code {
            match self.at(a) {
                Some((i, J | Jal)) if !self.floating => {
                    t.insert(i.jump_target(a));
                }
                Some((i, Beq | Bne | Blez | Bgtz | Bltz | Bgez | Bltzal | Bgezal)) => {
                    t.insert(i.branch_target(a));
                }
                Some((i, k @ (Addiu | Addi | Ori))) => {
                    // A return point built in ra (ra_const) is entered as a call returns to it.
                    t.extend(self.ra_const(a, i, k));
                }
                _ => {}
            }
        }
        t
    }

    /// Where the conditional branch at `a` to guest `t` points: `t` itself, or a nearby `j t`
    /// when `t` is out of range.
    fn near(&mut self, a: u32, t: u32) -> To {
        if !self.long.contains(&a) {
            return To::Guest(t);
        }
        let l = self.label();
        let mut s = Code::default();
        s.def(l);
        s.jump(2, To::Guest(t), a);
        s.push(NOP);
        self.stub(s);
        To::Label(l)
    }

    fn stub(&mut self, s: Code) {
        if self.stubs.is_empty() {
            self.oldest = self.out.w.len();
        }
        self.stub_words += s.w.len();
        self.stubs.push(s);
    }

    fn flush(&mut self) {
        if !self.stubs.is_empty() {
            self.cold.0 += self.stub_words;
            self.cold.1 += 1;
        }
        for s in std::mem::take(&mut self.stubs) {
            self.out.append(s);
        }
        self.stub_words = 0;
    }

    /// After an unconditional transfer: the pending stubs go here if their block is full.
    fn run_end(&mut self) {
        if !self.stubs.is_empty() && self.out.w.len() - self.oldest + self.stub_words > COLD_BLOCK {
            self.flush();
        }
    }

    fn far_flush(&mut self) {
        if !self.stubs.is_empty() && self.out.w.len() - self.oldest + self.stub_words > FLUSH_WORDS {
            let over = self.label();
            self.out.jump(2, To::Label(over), 0);
            self.out.push(NOP);
            self.flush();
            self.out.def(over);
        }
    }

    fn exec(&mut self, pc: u32, word: u32, why: Reason) -> Code {
        self.runtime.insert(pc, why);
        let mut c = Code::default();
        c.host_call(pc, word);
        c
    }

    fn resume(&mut self, pc: u32, why: Reason) -> Code {
        self.runtime.insert(pc, why);
        let mut c = Code::default();
        c.host_call(pc | RESUME, self.exe.word(pc).unwrap_or(0));
        c
    }

    /// The main-pass instruction at `a` (with its delay slot). Returns where the guest continues
    /// in straight line, if it can.
    fn insn(&mut self, a: u32) -> Option<u32> {
        if let Some(&h) = self.hooks.get(&a) {
            // The runtime continues where the recipe's code would, through the dispatch table.
            self.runtime.insert(a, Reason::Hook);
            // v_wait's pass costs the runtime what the translated loop's pass would.
            let arg1 = if h.id == hooks::VSYNC_WAIT { u32::from(self.loop_cost(a, h.args[1])) } else { h.args[1] };
            self.out.host_call(a | HOOK, h.id);
            self.out.push(h.args[0]);
            self.out.push(arg1);
            return None;
        }
        let Some((i, k)) = self.at(a) else {
            let c = self.resume(a, Reason::Outside);
            self.out.append(c);
            return None;
        };
        if k.has_delay_slot() {
            return self.transfer(a, i, k);
        }
        if let Some((pcs, lo, hi)) = self.run_from(a) {
            let c = self.group(&pcs, lo, hi);
            self.out.append(c);
            self.skip_to = pcs[pcs.len() - 1] + 4;
            return Some(self.skip_to);
        }
        let (c, falls) = if self.hz.sites.contains_key(&a) {
            (self.resume(a, Reason::Hazard), false)
        } else {
            self.plain(a, i, k)
        };
        self.out.append(c);
        falls.then_some(a + 4)
    }

    /// The translated code address the `lui`/`addiu`/`ori` at `pc` completes in `ra`. Its host
    /// address is written instead: a translated `jr ra` returns natively, and some hand-written
    /// code makes its own return points (Crash 2's 8003B134 sets ra to its loop head 8003B38C).
    /// Not a lone `lui`: its value is the high half of an address the next instruction completes.
    fn ra_const(&self, pc: u32, i: Insn, k: Kind) -> Option<u32> {
        if self.floating || i.rt() != RA as usize {
            return None;
        }
        let img = Image(self.exe, &[], false);
        let v = match k {
            Kind::Addiu | Kind::Addi => img.const_of(i.rs(), pc)?.wrapping_add(i.simm()),
            Kind::Ori => img.const_of(i.rs(), pc)? | i.imm(),
            _ => return None,
        };
        self.d.code.contains(&v).then_some(v)
    }

    /// [`simple`], but not for an instruction [`Em::ra_const`] rewrites.
    fn simple_at(&self, pc: u32, i: Insn, k: Kind) -> Option<u32> {
        if self.ra_const(pc, i, k).is_some() { None } else { simple(i, k) }
    }

    /// A non-transfer instruction. Returns its code and whether it continues at the next one.
    fn plain(&mut self, pc: u32, i: Insn, k: Kind) -> (Code, bool) {
        use Kind::*;
        if let Some(t) = self.ra_const(pc, i, k) {
            let mut c = Code::default();
            c.abs(RA, To::Guest(t), pc);
            return (c, true);
        }
        if let Some(w) = simple(i, k) {
            let mut c = Code::default();
            c.simple(w, pc);
            return (c, true);
        }
        let u = Uses::of(i, k);
        if (u.read | u.write) & GUEST_K != 0 && k != Invalid {
            return (self.exec(pc, i.0, Reason::GuestK), true);
        }
        let c = match k {
            Div | Divu => self.div(pc, i, k == Div),
            Lb | Lh | Lwl | Lw | Lbu | Lhu | Lwr | Sb | Sh | Swl | Sw | Swr => self.mem(pc, i),
            Mfc0 | Mtc0 | Rfe => self.exec(pc, i.0, Reason::Cop0),
            Mfc2 | Cfc2 | Mtc2 | Ctc2 | Gte | Lwc2 | Swc2 => match self.cop2(pc, i, k) {
                Some(c) => c,
                None => self.exec(pc, i.0, Reason::Cop2),
            },
            Syscall => self.exec(pc, i.0, Reason::Syscall),
            Break => self.exec(pc, i.0, Reason::Break),
            _ => return (self.resume(pc, Reason::Invalid), false),
        };
        (c, true)
    }

    /// `div`/`divu rs,rt` with the R3000A's results for the cases the Allegrex leaves undefined.
    fn div(&mut self, pc: u32, i: Insn, signed: bool) -> Code {
        let (rs, rt) = (i.rs() as u32, i.rt() as u32);
        let (zero, back) = (self.label(), self.label());
        let mut c = Code::default();
        let mut z = Code::default();
        c.branch(beq(rt, ZR), To::Label(zero), pc);
        z.def(zero);
        z.push(mthi(rs));
        if signed {
            let minus1 = self.label();
            c.push(addiu(K1R, rt, 1));
            c.branch(beq(K1R, ZR), To::Label(minus1), pc);
            c.push(NOP);
            // rs / 0: lo = rs < 0 ? 1 : -1
            z.push(srl(K1R, rs, 31));
            z.push(sll(K1R, K1R, 1));
            z.push(addiu(K1R, K1R, 0xFFFF));
            // rs / -1: lo = -rs (i32::MIN stays), hi = 0
            let mut m = Code::default();
            m.def(minus1);
            m.push(mthi(ZR));
            m.push(subu(K1R, ZR, rs));
            m.push(mtlo(K1R));
            m.jump(2, To::Label(back), pc);
            m.push(NOP);
            self.stub(m);
        } else {
            c.push(NOP);
            z.push(addiu(K1R, ZR, 0xFFFF));
        }
        z.push(mtlo(K1R));
        z.jump(2, To::Label(back), pc);
        z.push(NOP);
        self.stub(z);
        c.push(i.0);
        c.def(back);
        c
    }

    /// A load or store through the guest address in `k1`.
    fn mem(&mut self, pc: u32, i: Insn) -> Code {
        // Same operation and data register, base k1.
        let op = (i.0 & 0xFC1F_0000) | (K1R << 21);
        self.access(pc, i, op, &[], &[])
    }

    /// The GTE without the runtime: moves, loads and stores work on the registers at
    /// [`layout::GTE`] with the hardware's write rules, commands go through [`layout::GTE_CMD`] or
    /// their entry in [`layout::GTE_OWN`].
    /// `None` for the registers whose rules need the runtime (IRGB, ORGB, LZCS, FLAG writes; the
    /// ORGB/IRGB reads; SXYP and the ignored writes through memory).
    fn cop2(&mut self, pc: u32, i: Insn, k: Kind) -> Option<Code> {
        use Kind::*;
        let (rs, rt, rd) = (i.rs() as u32, i.rt() as u32, i.rd() as u32);
        let data = |r: u32| k0_off(layout::GTE + gte::data(r));
        let ctrl = |r: u32| k0_off(layout::GTE + gte::ctrl(r));
        let gpr = |r: u32| k0_off(layout::CTX + ctx::GPR + 4 * r);
        let mut c = Code::default();
        match k {
            Mfc2 => match rd {
                28 | 29 => return None,
                _ if rt == 0 => c.push(NOP),
                15 => c.push(lw(rt, data(14), K0R)),
                _ => c.push(lw(rt, data(rd), K0R)),
            },
            Cfc2 => c.push(if rt == 0 { NOP } else { lw(rt, ctrl(rd), K0R) }),
            Mtc2 => match rd {
                28 | 30 => return None,
                29 | 31 => c.push(NOP),
                15 => {
                    for r in [13, 14] {
                        c.push(lw(K1R, data(r), K0R));
                        c.push(sw(K1R, data(r - 1), K0R));
                    }
                    c.push(sw(rt, data(14), K0R));
                }
                1 | 3 | 5 | 8..=11 => {
                    c.push(seh(K1R, rt));
                    c.push(sw(K1R, data(rd), K0R));
                }
                7 | 16..=19 => {
                    c.push(andi(K1R, rt, 0xFFFF));
                    c.push(sw(K1R, data(rd), K0R));
                }
                _ => c.push(sw(rt, data(rd), K0R)),
            },
            Ctc2 => match rd {
                31 => return None,
                4 | 12 | 20 | 26 | 27 | 29 | 30 => {
                    c.push(seh(K1R, rt));
                    c.push(sw(K1R, ctrl(rd), K0R));
                }
                _ => c.push(sw(rt, ctrl(rd), K0R)),
            },
            Gte => {
                let cmd = i.0 & 0x01FF_FFFF;
                c.push(sw(RA, gpr(RA), K0R));
                if let Some(&(entry, ..)) = layout::GTE_OWN.iter().find(|&&(_, m, v)| cmd & m == v) {
                    c.push(jal(entry));
                    c.push(NOP);
                } else {
                    let entry = layout::GTE_WORD.iter().find(|&&(_, m, v)| cmd & m == v);
                    c.push(lui(K1R, (cmd >> 16) as u16));
                    c.push(jal(entry.map_or(layout::GTE_CMD, |e| e.0)));
                    c.push(ori(K1R, K1R, cmd as u16));
                }
            }
            // The word is loaded into k1, then stored by the register's rule.
            Lwc2 => {
                let op = match rt {
                    15 | 28..=31 => return None,
                    1 | 3 | 5 | 8..=11 => lh(K1R, 0, K1R),
                    7 | 16..=19 => lhu(K1R, 0, K1R),
                    _ => lw(K1R, 0, K1R),
                };
                c = self.access(pc, i, op, &[sw(K1R, data(rt), K0R)], &[]);
            }
            // The register goes through a guest register parked in its CTX slot.
            Swc2 => {
                let src = match rt {
                    28 | 29 => return None,
                    15 => 14,
                    r => r,
                };
                let t = if rs == 1 { 2 } else { 1 };
                c.push(sw(t, gpr(t), K0R));
                c.push(lw(t, data(src), K0R));
                c.append(self.access(pc, i, sw(t, 0, K1R), &[], &[lw(t, gpr(t), K0R)]));
                c.push(lw(t, gpr(t), K0R));
            }
            _ => return None,
        }
        Some(c)
    }

    /// The loads and stores from `a` on that share a base register, with only plain copied words
    /// between them (they leave `k1` alone): their addresses, `a` to the last of them, and the
    /// lowest and highest offset, when there are at least two. None of them is a transfer target,
    /// a hook or a hazard, nor writes the base before the last (a load into it ends the group).
    fn run_from(&self, a: u32) -> Option<(Vec<u32>, i32, i32)> {
        use Kind::*;
        let mem = |k: Kind| matches!(k, Lb | Lh | Lwl | Lw | Lbu | Lhu | Lwr | Sb | Sh | Swl | Sw | Swr);
        let ok = |p: u32| self.first(p) && !self.hz.sites.contains_key(&p) && !self.hooks.contains_key(&p);
        let (i, k) = self.at(a)?;
        let u = Uses::of(i, k);
        let base = i.rs();
        // An access that writes its own base is a group of one.
        if !mem(k) || !ok(a) || (u.read | u.write) & GUEST_K != 0 || u.write & (1 << base) != 0 {
            return None;
        }
        let (mut lo, mut hi) = (i.simm() as i32, i.simm() as i32);
        let (mut pcs, mut last) = (vec![a], 0);
        let mut p = a;
        loop {
            p += 4;
            if !ok(p) || self.targets.contains(&p) {
                break;
            }
            let Some((i, k)) = self.at(p) else { break };
            let u = Uses::of(i, k);
            if mem(k) && i.rs() == base && (u.read | u.write) & GUEST_K == 0 {
                let o = i.simm() as i32;
                // The out-of-line copy (with its accesses' own stubs) stays within branch range.
                if hi.max(o) - lo.min(o) > 0x7FFF || pcs.len() >= GROUP_MAX {
                    break;
                }
                (lo, hi) = (lo.min(o), hi.max(o));
                pcs.push(p);
                last = pcs.len() - 1;
                if u.write & (1 << base) != 0 {
                    break;
                }
            } else if self.simple_at(p, i, k).is_some() && u.write & (1 << base) == 0 {
                pcs.push(p);
            } else {
                break;
            }
        }
        pcs.truncate(last + 1);
        (last > 0).then_some((pcs, lo, hi))
    }

    /// The group `pcs` of [`Em::run_from`] with one check of its base: when the addresses from
    /// `lo` to `hi` past it are RAM in one 2 MB mirror, `k1` is set once and every load and store
    /// is its own host word on it. Otherwise an out-of-line copy runs the group as
    /// [`Em::access`] would; a jump into the group lands in that copy.
    fn group(&mut self, pcs: &[u32], lo: i32, hi: i32) -> Code {
        let a = pcs[0];
        let rs = Insn(self.exe.word(a).unwrap_or(0)).rs() as u32;
        let (cold, back) = (self.label(), self.label());
        let mut c = Code::default();
        if hi > lo {
            // The last address's low 21 bits are below the span: the group crosses 2 MB.
            c.push(addiu(K1R, rs, hi as u16));
            c.push(ext(K1R, K1R, 0, 21));
            c.push(sltiu(K1R, K1R, (hi - lo) as u16));
            c.branch(bne(K1R, ZR), To::Label(cold), a);
        }
        c.push(addiu(K1R, rs, lo as u16));
        c.push(sll(K1R, K1R, 8));
        c.branch(bltz(K1R), To::Label(cold), a);
        c.push(srl(K1R, K1R, 8));
        c.push(ins(K1R, K0R, 21, 11));
        let mut s = Code::default();
        s.def(cold);
        for &p in pcs {
            let Some((i, k)) = self.at(p) else { continue };
            if p != a {
                let l = self.label();
                s.def(l);
                self.map_at.push((p, l));
            }
            match simple(i, k) {
                Some(w) => {
                    c.simple(w, p);
                    s.simple(w, p);
                }
                None => {
                    c.simple((i.0 & 0xFC1F_0000) | (K1R << 21) | (i.simm() as i32 - lo) as u16 as u32, p);
                    s.append(self.mem(p, i));
                }
            }
        }
        s.jump(2, To::Label(back), a);
        s.push(NOP);
        self.stub(s);
        c.def(back);
        c
    }

    /// `op` (base `k1`, offset 0) on the host address of the guest address of the load or store
    /// `i`, then `then`. RAM and the scratchpad are reached directly; anything else runs `i` in
    /// the runtime after `undo` (a plain load through [`layout::IO_READ`], a plain store through
    /// [`layout::IO_WRITE`]).
    fn access(&mut self, pc: u32, i: Insn, op: u32, then: &[u32], undo: &[u32]) -> Code {
        let (rs, off) = (i.rs() as u32, i.imm() as u16);
        let (slow, io, back) = (self.label(), self.label(), self.label());
        let mut c = Code::default();
        c.simple(addiu(K1R, rs, off), pc);
        c.push(sll(K1R, K1R, 8));
        c.branch(bltz(K1R), To::Label(slow), pc);
        c.push(srl(K1R, K1R, 8));
        c.push(ins(K1R, K0R, 21, 11));
        c.push(op);
        then.iter().for_each(|&w| c.push(w));
        c.def(back);
        // Bit 23 set: scratchpad if bits 10-23 are 0x2000, else the runtime.
        let mut s = Code::default();
        s.def(slow);
        s.push(srl(K1R, K1R, 10));
        s.push(xori(K1R, K1R, 0x2000));
        s.branch(bne(K1R, ZR), To::Label(io), pc);
        s.simple(addiu(K1R, rs, off), pc);
        s.push(andi(K1R, K1R, 0x3FF));
        s.push(addu(K1R, K1R, K0R));
        s.push(op | k0_off(SCRATCH) as u32);
        then.iter().for_each(|&w| s.push(w));
        s.jump(2, To::Label(back), pc);
        s.push(NOP);
        s.def(io);
        // A plain load goes to IO_READ, which leaves the loaded value in CTX. Its guest address
        // is in k1 from the jal's slot: an interrupt taken first runs the site again from its
        // first word, as a host call's (the slot reads ra after the link, so not on ra).
        let rt = i.rt() as u32;
        let plain = matches!(i.0 >> 26, 0x20 | 0x21 | 0x23 | 0x24 | 0x25);
        if plain && then.is_empty() && undo.is_empty() && rt != 0 && rt != RA && rs != RA {
            s.push(sw(RA, k0_off(layout::CTX + ctx::GPR + 4 * RA), K0R));
            s.push(jal(layout::IO_READ));
            s.simple(addiu(K1R, rs, off), pc);
            s.guest(pc);
            s.simple(i.0, pc);
            s.jump(2, To::Label(back), pc);
            s.push(lw(rt, k0_off(layout::CTX + ctx::GPR + 4 * rt), K0R));
            self.stub(s);
            return c;
        }
        // A plain store goes to IO_WRITE with its value in CTX (the runtime keeps only the
        // caller-saved registers there); the site's sw ra is where an interrupt comes back to.
        let store = matches!(i.0 >> 26, 0x28 | 0x29 | 0x2B);
        if store && then.is_empty() && undo.is_empty() && rs != RA {
            if rt != 0 && rt != RA {
                s.push(sw(rt, k0_off(layout::CTX + ctx::GPR + 4 * rt), K0R));
            }
            s.push(sw(RA, k0_off(layout::CTX + ctx::GPR + 4 * RA), K0R));
            s.push(jal(layout::IO_WRITE));
            s.simple(addiu(K1R, rs, off), pc);
            s.guest(pc);
            s.simple(i.0, pc);
            s.jump(2, To::Label(back), pc);
            s.push(NOP);
            self.stub(s);
            return c;
        }
        undo.iter().for_each(|&w| s.push(w));
        s.host_call_mem(pc, i.0);
        s.jump(2, To::Label(back), pc);
        s.push(NOP);
        self.stub(s);
        c
    }

    /// PS1 cycles charged for one pass of the loop `t..=a` (`a` the back-edge): the words every
    /// pass runs, plus [`RAM_READ`] for each read (writes go through the R3000A's write queue).
    /// Words that a forward branch or jump inside the loop passes over are a path some passes
    /// take, so they are not charged. Charging them made a decoder's pass (a switch over code
    /// lengths) cost all its cases: Exoddus's movie decoder took 2.5 times its PS1 time and the
    /// movies dropped frames.
    ///
    /// A counted wait loop must last as long as on the PS1. PsyQ's `VSync` gives each vblank it
    /// waits for 32768 passes of 14 words and 3 reads, and prints "VSync: timeout" if they run
    /// out first. At 32 cycles a pass that is 31 ms, against a 20 ms PAL frame. With 2 cycles a
    /// read and the skipped words not charged it was 22 cycles, 21 ms: Sheep Dog's VSync timed
    /// out every frame and the game ran unpaced.
    fn loop_cost(&self, t: u32, a: u32) -> u16 {
        use Kind::*;
        let mut skipped = Vec::new();
        for w in (t..a).step_by(4).filter(|w| self.d.code.contains(w)) {
            let to = match self.at(w) {
                Some((i, J)) if !self.floating => i.jump_target(w),
                Some((i, Beq | Bne | Blez | Bgtz | Bltz | Bgez)) => i.branch_target(w),
                _ => continue,
            };
            if to > w + 8 && to <= a {
                skipped.push(w + 8..to);
            }
        }
        let mut cost = 0u32;
        for w in (t..=a + 4).step_by(4).filter(|w| self.d.code.contains(w) && !skipped.iter().any(|r| r.contains(w))) {
            let read = self.at(w).is_some_and(|(_, k)| matches!(k, Lb | Lh | Lwl | Lw | Lbu | Lhu | Lwr | Lwc2));
            cost += if read { 1 + RAM_READ } else { 1 };
        }
        cost.clamp(1, MAX_LOOP_COST) as u16
    }

    /// `lw k1,BUDGET(k0); addiu k1,k1,-cost; bltz k1,irq; sw k1,BUDGET(k0)` with the host call
    /// out of line.
    fn poll(&mut self, pc: u32, cost: u16) {
        let (irq, back) = (self.label(), self.label());
        let budget = k0_off(layout::CTX + ctx::BUDGET);
        self.out.push(lw(K1R, budget, K0R));
        self.out.push(addiu(K1R, K1R, cost.wrapping_neg()));
        self.out.branch(bltz(K1R), To::Label(irq), pc);
        self.out.push(sw(K1R, budget, K0R));
        self.out.def(back);
        let mut s = Code::default();
        s.def(irq);
        s.host_call(pc, NOP);
        s.jump(2, To::Label(back), pc);
        s.push(NOP);
        self.stub(s);
    }

    /// `k1` <- host address for the guest address in `src` (also saved to `ctx::TARGET`). With
    /// `host`, a value that is already host code goes as it is, as the runtime's `host_of` does:
    /// a translated call writes a host ra, and code that keeps ra in memory and returns through
    /// another register (Spyro 2's 8001E3E8, `jr t7`) would index the table with it and land on
    /// whatever guest address shares its low 21 bits. Host addresses are 0x08000000..0x0FFFFFFF,
    /// top five bits 00001; no guest segment has them (00000, 00011, 10000, 10100, 10111).
    fn dispatch(&mut self, src: u32, host: bool, pc: u32) {
        let done = host.then(|| self.label());
        let o = &mut self.out;
        o.push(sw(src, k0_off(layout::CTX + ctx::TARGET), K0R));
        if let Some(done) = done {
            o.push(srl(K1R, src, 27));
            o.push(addiu(K1R, K1R, 0xFFFF));
            o.branch(beq(K1R, 0), To::Label(done), pc);
            o.push(addu(K1R, src, 0));
        }
        o.push(ext(K1R, src, 0, 21));
        o.push(addu(K1R, K1R, K0R));
        o.push(lw(K1R, k0_off(layout::TABLE), K1R));
        if let Some(done) = done {
            o.def(done);
        }
    }

    /// The host delay slot after a transfer: the guest slot's simple word, or a `nop` when its
    /// code went before the transfer.
    fn slot_word(&mut self, simple_slot: Option<u32>, s: u32) {
        match simple_slot {
            Some(w) => self.out.simple(w, s),
            None => self.out.push(NOP),
        }
    }

    /// A branch or jump at `a` and its delay slot.
    fn transfer(&mut self, a: u32, i: Insn, k: Kind) -> Option<u32> {
        use Kind::*;
        let s = a + 4;
        let Some((si, sk)) = self.at(s) else {
            let c = self.resume(a, Reason::Outside);
            self.out.append(c);
            return None;
        };
        let (bu, su) = (Uses::of(i, k), Uses::of(si, sk));
        let link = match k {
            Jal | Bltzal | Bgezal => 1 << RA,
            Jalr => 1 << i.rd(),
            _ => 0,
        };
        let simple_slot = self.simple_at(s, si, sk);
        let clobbers = su.write & bu.read != 0;
        let why = if sk.has_delay_slot() {
            Some(Reason::BranchInSlot)
        } else if self.hz.sites.contains_key(&s) {
            Some(Reason::Hazard)
        } else if (bu.read | bu.write) & GUEST_K != 0 {
            Some(Reason::GuestK)
        } else if (su.read | su.write) & link != 0 {
            Some(Reason::LinkInSlot)
        } else if link != 0 && clobbers && simple_slot.is_none() {
            // A conditional call cannot duplicate its slot: the copy would be the return point.
            Some(Reason::SlotClobbers)
        } else {
            None
        };
        if let Some(why) = why {
            let c = self.resume(a, why);
            self.out.append(c);
            return None;
        }
        // The slot's code when it is not one plain word.
        let slot = |em: &mut Self| em.plain(s, si, sk).0;
        let uncond = matches!(k, J) || (k == Beq && i.rs() == i.rt()) || (k == Bgez && i.rs() == 0);
        let bal = k == Bgezal && i.rs() == 0;
        match k {
            Jr if i.rs() == RA as usize && self.leaf.contains(&a) => {
                if let Some(w) = simple_slot {
                    self.out.push(jr(RA));
                    self.out.simple(w, s);
                } else if clobbers {
                    self.out.push(sw(RA, k0_off(layout::CTX + ctx::JUMP), K0R));
                    let c = slot(self);
                    self.out.append(c);
                    self.out.push(lw(K1R, k0_off(layout::CTX + ctx::JUMP), K0R));
                    self.out.push(jr(K1R));
                    self.out.push(NOP);
                } else {
                    let c = slot(self);
                    self.out.append(c);
                    self.out.push(jr(RA));
                    self.out.push(NOP);
                }
                None
            }
            Jr | Jalr => {
                // Only `ra` takes a host link (a translated `jr ra` returns natively). Another
                // link register is data to the guest: Crash 2's GOOL code ends its native pieces
                // with `jalr s5,ra`, and s5, the bytecode pointer, must be 8 past the guest jalr.
                let rd = if k == Jalr { i.rd() as u32 } else { ZR };
                let fin = if rd == RA { jalr(RA, K1R) } else { jr(K1R) };
                let link = |o: &mut Code| {
                    if rd != ZR && rd != RA {
                        o.guest_pair(rd, a + 8, true);
                    }
                };
                let rs = i.rs() as u32;
                // A switch table holds guest addresses only.
                let host = !matches!(self.d.jumps.get(&a), Some(JumpReg::Table { .. }));
                if let Some(w) = simple_slot {
                    self.dispatch(rs, host, a);
                    link(&mut self.out);
                    self.out.push(fin);
                    self.out.simple(w, s);
                } else if !clobbers {
                    let c = slot(self);
                    self.out.append(c);
                    self.dispatch(rs, host, a);
                    link(&mut self.out);
                    self.out.push(fin);
                    self.out.push(NOP);
                } else {
                    self.dispatch(rs, host, a);
                    self.out.push(sw(K1R, k0_off(layout::CTX + ctx::JUMP), K0R));
                    let c = slot(self);
                    self.out.append(c);
                    self.out.push(lw(K1R, k0_off(layout::CTX + ctx::JUMP), K0R));
                    link(&mut self.out);
                    self.out.push(fin);
                    self.out.push(NOP);
                }
                (k == Jalr).then_some(a + 8)
            }
            Jal | Bgezal if k == Jal || bal => {
                let t = if k == Jal { i.jump_target(a) } else { i.branch_target(a) };
                let inside = !(self.floating && k == Jal) && self.d.code.contains(&t);
                if simple_slot.is_none() {
                    let c = slot(self);
                    self.out.append(c);
                }
                if inside {
                    self.out.jump(3, To::Guest(t), a);
                } else {
                    if k == Jal {
                        self.out.target_pair(K1R, t, a);
                    } else {
                        self.out.guest_pair(K1R, t, true);
                    }
                    self.dispatch(K1R, false, a);
                    self.out.push(jalr(RA, K1R));
                }
                self.slot_word(simple_slot, s);
                Some(a + 8)
            }
            _ => {
                let t = if k == J { i.jump_target(a) } else { i.branch_target(a) };
                if self.floating && k == J {
                    // Through the table, to whatever the word RAM holds names.
                    if t <= a && self.d.code.contains(&t) {
                        let cost = self.loop_cost(t, a);
                        self.poll(a, cost);
                    }
                    if simple_slot.is_none() {
                        let c = slot(self);
                        self.out.append(c);
                    }
                    self.out.target_pair(K1R, t, a);
                    self.dispatch(K1R, false, a);
                    self.out.push(jr(K1R));
                    self.slot_word(simple_slot, s);
                    return None;
                }
                if !self.d.code.contains(&t) {
                    // Out of the code (another floating unit, another image): through the table to
                    // the guest address, the slot run on both paths as the guest runs it. The
                    // branch is inverted to step over the exit.
                    let inv = match k {
                        Beq | Bne | Blez | Bgtz => Some(i.0 ^ 1 << 26),
                        Bltz | Bgez => Some(i.0 ^ 1 << 16),
                        _ => None,
                    };
                    if link != 0 || (clobbers && simple_slot.is_none()) || (!uncond && inv.is_none()) {
                        let c = self.resume(a, Reason::Outside);
                        self.out.append(c);
                        return None;
                    }
                    if t <= a {
                        let cost = self.loop_cost(t, a);
                        self.poll(a, cost);
                    }
                    if simple_slot.is_none() {
                        let c = slot(self);
                        self.out.append(c);
                    }
                    let skip = (!uncond).then(|| self.label());
                    if let (Some(skip), Some(inv)) = (skip, inv) {
                        self.out.branch(inv, To::Label(skip), a);
                        self.slot_word(simple_slot, s);
                    }
                    self.out.guest_pair(K1R, t, self.floating);
                    self.dispatch(K1R, false, a);
                    self.out.push(jr(K1R));
                    match skip {
                        Some(skip) => {
                            self.out.push(NOP);
                            self.out.def(skip);
                            return Some(a + 8);
                        }
                        None => {
                            self.slot_word(simple_slot, s);
                            return None;
                        }
                    }
                }
                if t <= a && link == 0 {
                    let cost = self.loop_cost(t, a);
                    self.poll(a, cost);
                }
                if uncond {
                    if simple_slot.is_none() {
                        let c = slot(self);
                        self.out.append(c);
                    }
                    self.out.jump(2, To::Guest(t), a);
                    self.slot_word(simple_slot, s);
                    return None;
                }
                let word = i.0;
                if let Some(w) = simple_slot {
                    let to = self.near(a, t);
                    self.out.branch(word, to, a);
                    self.out.simple(w, s);
                } else if !clobbers {
                    let c = slot(self);
                    self.out.append(c);
                    let to = self.near(a, t);
                    self.out.branch(word, to, a);
                    self.out.push(NOP);
                } else {
                    let taken = self.label();
                    self.out.branch(word, To::Label(taken), a);
                    self.out.push(NOP);
                    let c = slot(self);
                    self.out.append(c);
                    let mut st = Code::default();
                    st.def(taken);
                    st.append(slot(self));
                    st.jump(2, To::Guest(t), a);
                    st.push(NOP);
                    self.stub(st);
                }
                Some(a + 8)
            }
        }
    }

    fn finish(self, base: u32) -> Result<Translation, Fail> {
        let Em { out, mut map, map_at, runtime, labels, cold, spans, .. } = self;
        let Code { mut w, fix, defs, pat, rel, tgt } = out;
        let mut at = vec![usize::MAX; labels];
        for (l, i) in defs {
            at[l] = i;
        }
        map.extend(map_at.into_iter().map(|(g, l)| (g, at[l])));
        debug_assert!(!at.contains(&usize::MAX), "label used but never placed");
        let host = |i: usize| base + 4 * i as u32;
        let mut far = Vec::new();
        let mut relocs: Vec<(u32, Reloc)> = rel.into_iter().map(|(i, r)| (host(i), r)).collect();
        for (i, fx, to, pc) in fix {
            let t = match to {
                To::Guest(g) => *map
                    .get(&g)
                    .ok_or(Fail::Error(EmitError::Target { pc, target: g }))?,
                To::Label(l) => at[l],
            };
            match fx {
                Fx::Branch => {
                    let words = t as i64 - i as i64 - 1;
                    if words < i16::MIN as i64 || words > i16::MAX as i64 {
                        match to {
                            To::Guest(_) => far.push((pc, words)),
                            To::Label(_) => return Err(Fail::Error(EmitError::Range { pc, words })),
                        }
                    }
                    w[i] |= words as u16 as u32;
                }
                Fx::Jump => {
                    w[i] |= (host(t) >> 2) & 0x03FF_FFFF;
                    relocs.push((host(i), Reloc::Jump));
                }
                Fx::Abs => {
                    w[i] |= host(t) >> 16;
                    w[i + 1] |= host(t) & 0xFFFF;
                    relocs.push((host(i), Reloc::HostPair));
                }
            }
        }
        if !far.is_empty() {
            return Err(Fail::Far(far));
        }
        let mut patch: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for (i, pc) in pat {
            patch.entry(pc).or_default().push(host(i));
        }
        Ok(Translation {
            base,
            words: w,
            map: map.into_iter().map(|(g, i)| (g, host(i))).collect(),
            runtime,
            patch,
            cold,
            spans: spans.into_iter().map(|(g, s, e)| (g, host(s), host(e))).collect(),
            relocs: {
                relocs.sort_unstable_by_key(|r| r.0);
                relocs
            },
            targets: tgt.into_iter().map(|(i, pc)| (host(i), pc)).collect(),
        })
    }
}

impl Translation {
    /// Host address of the translation of guest `pc`.
    pub fn host(&self, pc: u32) -> Option<u32> {
        self.map.get(&pc).copied()
    }

    /// This code with its guest addresses `guest` bytes further and its host code at `base`: what
    /// [`translate`] of the same words at those addresses gives, for code that does not name
    /// itself by absolute address ([`translate_floating`]). A `j`/`jal` stays in its 256 MB
    /// segment, so `base` must be in the one of [`Translation::base`].
    pub fn moved(&self, guest: u32, base: u32) -> Translation {
        let host = base.wrapping_sub(self.base);
        let at = |h: u32| h.wrapping_add(host);
        let mut words = self.words.clone();
        let pair = |w: &mut [u32], i: usize, d: u32| {
            let v = (w[i] << 16 | w[i + 1] & 0xFFFF).wrapping_add(d);
            w[i] = w[i] & 0xFFFF_0000 | v >> 16;
            w[i + 1] = w[i + 1] & 0xFFFF_0000 | v & 0xFFFF;
        };
        for &(h, r) in &self.relocs {
            let i = ((h - self.base) / 4) as usize;
            match r {
                Reloc::Guest => words[i] = words[i].wrapping_add(guest),
                Reloc::GuestPair => pair(&mut words, i, guest),
                Reloc::HostPair => pair(&mut words, i, host),
                Reloc::Jump => {
                    let t = (words[i] & 0x03FF_FFFF) << 2 | self.base & 0xF000_0000;
                    words[i] = words[i] & 0xFC00_0000 | (at(t) >> 2) & 0x03FF_FFFF;
                }
            }
        }
        let g = |a: u32| a.wrapping_add(guest);
        Translation {
            base,
            words,
            map: self.map.iter().map(|(&a, &h)| (g(a), at(h))).collect(),
            runtime: self.runtime.iter().map(|(&a, &r)| (g(a), r)).collect(),
            patch: self.patch.iter().map(|(&a, v)| (g(a), v.iter().map(|&h| at(h)).collect())).collect(),
            cold: self.cold,
            spans: self.spans.iter().map(|&(a, s, e)| (g(a), at(s), at(e))).collect(),
            relocs: self.relocs.iter().map(|&(h, r)| (at(h), r)).collect(),
            targets: self.targets.iter().map(|&(h, pc)| (at(h), g(pc))).collect(),
        }
    }

    /// Dispatch-table entries: (host address of the entry, value).
    pub fn table(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        self.map.iter().map(|(&g, &h)| (layout::table_entry(g), h))
    }
}

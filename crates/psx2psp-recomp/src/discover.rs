//! Code discovery in a PS-X EXE: which words are instructions, where functions start, where
//! register jumps go.
//!
//! Recursive descent from the entry point, then from every `addiu`/`addi sp,sp,-N` prologue the
//! descent has not reached (walked back over the instructions that precede it in the same
//! block), until nothing new is found. Register jumps are resolved where the code says where
//! they go: compiler switch tables (`sltiu` bound, `sll 2`, `lui`+`addu`, `lw`, `jr`) and
//! constants built with `lui`/`addiu`/`ori`. What stays unresolved (calls through pointers) goes
//! through the run-time dispatch table.
//!
//! Every path stops at the first word that does not decode in canonical form: such a word is
//! reported, never guessed.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use crate::exe::PsxExe;
use crate::insn::{Insn, Isa, Kind, Uses, decode};

const RA: usize = 31;
const JR_RA: u32 = 0x03E0_0008;
/// Instructions a leaf found after a return may run straight before its own `jr ra`.
const LEAF_MAX: u32 = 64;

/// `a` holds a non-zero word right after a `jr ra`, its delay slot and up to 3 zero words.
fn after_return(exe: &PsxExe, a: u32) -> bool {
    if exe.word(a).is_none_or(|w| w == 0) {
        return false;
    }
    for pad in 0..4u32 {
        let slot = a.wrapping_sub(4 * (pad + 1));
        if exe.word(slot.wrapping_sub(4)) == Some(JR_RA) {
            return true;
        }
        if exe.word(slot) != Some(0) {
            return false;
        }
    }
    false
}

/// `a` holds a non-zero word right after a transfer that does not fall through (`jr ra`, `j`,
/// `b`, `jr`), its delay slot and any number of zero words (Crash 2's rasterizer 800409F4,
/// reached through a table, follows 203 of them).
fn after_end(img: Image, a: u32) -> bool {
    if img.0.word(a).is_none_or(|w| w == 0) {
        return false;
    }
    for pad in 0.. {
        let slot = a.wrapping_sub(4 * (pad + 1));
        if matches!(img.flow_at(slot.wrapping_sub(4)), Some(Flow::Return | Flow::Goto(_) | Flow::JumpReg)) {
            return true;
        }
        if img.0.word(slot) != Some(0) {
            return false;
        }
    }
    false
}

/// The members of every table of text pointers in the text (two or more aligned words in a row,
/// each pointing into it) that holds a function already found: a constructor list or a handler
/// table, whose other members are functions too wherever they sit (Sheep Dog's constructor list
/// 8008C9AC: its first member follows data, not a return). The tables of `file` (the disc file
/// the text was cut from) count as well.
fn tabled(exe: &PsxExe, file: Far, entries: &BTreeMap<u32, Origin>) -> HashSet<u32> {
    let mut out = HashSet::new();
    for b in std::iter::once(&exe.text[..]).chain(file.iter().map(|&(_, b)| b)) {
        let w: Vec<u32> = words(b).collect();
        let mut k = 0;
        while k < w.len() {
            let run: Vec<u32> = w[k..].iter().copied().take_while(|&w| w & 3 == 0 && exe.contains(w)).collect();
            if run.len() >= 2 && run.iter().any(|w| entries.contains_key(w)) {
                out.extend(&run);
            }
            k += run.len().max(1);
        }
    }
    out
}

/// The little-endian words of `b`.
fn words(b: &[u8]) -> impl Iterator<Item = u32> + '_ {
    b.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap()))
}

/// From `a`, every word decodes up to a `jr ra` (and its delay slot) within [`LEAF_MAX`].
fn leaf_shape(img: Image, a: u32) -> bool {
    (0..LEAF_MAX).map(|n| a.wrapping_add(4 * n)).try_for_each(|p| match img.at(p) {
        None => Err(false),
        Some(_) if img.0.word(p) == Some(JR_RA) => Err(img.at(p.wrapping_add(4)).is_some()),
        Some(_) => Ok(()),
    }) == Err(true)
}

/// Where control goes after an instruction (its delay slot aside).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    Next,
    /// Conditional branch: the target or the instruction after the delay slot.
    Branch(u32),
    /// Unconditional branch or `j`.
    Goto(u32),
    /// `jal`, `bal`, `bltzal`/`bgezal`: the callee, then the instruction after the delay slot.
    Call(u32),
    /// `jr ra`.
    Return,
    /// `jr` through any other register.
    JumpReg,
    /// `jalr`.
    CallReg,
}

/// The flow of `i` (decoded as `k`) at `pc`.
pub fn flow(i: Insn, k: Kind, pc: u32) -> Flow {
    use Kind::*;
    let bt = i.branch_target(pc);
    match k {
        Jr if i.rs() == RA => Flow::Return,
        Jr => Flow::JumpReg,
        Jalr => Flow::CallReg,
        J => Flow::Goto(i.jump_target(pc)),
        Jal => Flow::Call(i.jump_target(pc)),
        Beq if i.rs() == i.rt() => Flow::Goto(bt),
        Bgez if i.rs() == 0 => Flow::Goto(bt),
        Bltzal | Bgezal => Flow::Call(bt),
        Beq | Bne | Blez | Bgtz | Bltz | Bgez | Beql | Bnel | Blezl | Bgtzl => Flow::Branch(bt),
        _ => Flow::Next,
    }
}

/// Why a function was found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    Entry,
    Call,
    Prologue,
    /// Straight after a `jr ra` and its delay slot (and any zero padding), with a clean decode
    /// up to its own `jr ra`: a leaf reached only through a pointer built at run time.
    AfterReturn,
    /// A word of the image, or a constant the code builds, points at it; it starts where a
    /// previous function ends (after a `jr ra`, `j`, `b` or `jr` and its delay slot, and any zero
    /// padding), and every path from it decodes: a handler in a table of function pointers or
    /// stored in a state variable.
    Pointer,
}

/// A `jr` through a register other than `ra`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JumpReg {
    /// Switch table at `table`; `bounded` when its size comes from the `sltiu` guard.
    Table { table: u32, targets: Vec<u32>, bounded: bool },
    /// The register holds a constant (BIOS vectors 0xA0/0xB0/0xC0 among them).
    Const(u32),
    Unresolved(&'static str),
}

/// Why a path of the descent ended without a control transfer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeadEnd {
    /// The path left the text.
    Outside,
    /// A word that does not decode in canonical R3000A form.
    Invalid(u32),
}

/// The code of one executable as far as static analysis can see it.
#[derive(Clone, Debug, Default)]
pub struct Discovery {
    /// Every reached instruction, delay slots included.
    pub code: BTreeSet<u32>,
    /// Delay slot -> its branch or jump.
    pub delay_slots: BTreeMap<u32, u32>,
    pub entries: BTreeMap<u32, Origin>,
    /// Callee -> call sites (`jal`, `bal`, constant `jalr`); callees outside the text included.
    pub calls: BTreeMap<u32, Vec<u32>>,
    /// `jr` sites other than `jr ra`.
    pub jumps: BTreeMap<u32, JumpReg>,
    /// `jalr` sites and their target when constant.
    pub indirect_calls: BTreeMap<u32, Option<u32>>,
    /// Address where a path ended, and why.
    pub dead_ends: BTreeMap<u32, DeadEnd>,
    visited: HashSet<u32>,
}

/// One function: the instructions reachable from its entry without entering another function,
/// and its `jr ra` sites.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Body {
    pub insns: BTreeSet<u32>,
    pub returns: Vec<u32>,
}

/// Data outside the text that switch tables may be read from: (address of the first byte, bytes).
pub type Far<'a> = &'a [(u32, &'a [u8])];

/// Instruction access over the text, and the far data switch tables are read from.
#[derive(Clone, Copy)]
pub(crate) struct Image<'a>(pub &'a PsxExe, pub Far<'a>);

impl Image<'_> {
    /// The data word at `a`: the text's, else the far data's.
    fn data(self, a: u32) -> Option<u32> {
        if a & 3 != 0 {
            return None;
        }
        self.0.word(a).or_else(|| {
            self.1.iter().find_map(|&(base, b)| {
                let o = a.checked_sub(base)? as usize;
                b.get(o..o + 4).map(|w| u32::from_le_bytes(w.try_into().unwrap()))
            })
        })
    }

    /// The instruction at `a`, if inside the text and canonical.
    pub(crate) fn at(self, a: u32) -> Option<(Insn, Kind)> {
        let i = Insn(self.0.word(a)?);
        let k = decode(i, Isa::R3000A);
        (k != Kind::Invalid).then_some((i, k))
    }

    pub(crate) fn flow_at(self, a: u32) -> Option<Flow> {
        self.at(a).map(|(i, k)| flow(i, k, a))
    }

    /// True if `a` is the delay slot of a transfer that does not continue at `a + 4`.
    fn no_fall(self, a: u32) -> bool {
        matches!(
            self.flow_at(a.wrapping_sub(4)),
            Some(Flow::Goto(_) | Flow::Call(_) | Flow::Return | Flow::JumpReg | Flow::CallReg)
        )
    }

    /// The last instruction before `pc`, in the same straight-line block, that writes `reg`.
    fn find_def(self, reg: usize, pc: u32) -> Option<u32> {
        let mut a = pc.wrapping_sub(4);
        for _ in 0..32 {
            if !self.0.contains(a) || self.no_fall(a) {
                return None;
            }
            let (i, k) = self.at(a)?;
            if Uses::of(i, k).write & (1 << reg) != 0 {
                return Some(a);
            }
            a = a.wrapping_sub(4);
        }
        None
    }

    /// The value of `reg` just before `pc`, if built by a `lui`/`addiu`/`ori` chain.
    pub(crate) fn const_of(self, reg: usize, pc: u32) -> Option<u32> {
        self.const_at(reg, pc, 0)
    }

    fn const_at(self, reg: usize, pc: u32, depth: u32) -> Option<u32> {
        if reg == 0 {
            return Some(0);
        }
        if depth > 4 {
            return None;
        }
        let a = self.find_def(reg, pc)?;
        let (i, k) = self.at(a)?;
        match k {
            Kind::Lui => Some(i.imm() << 16),
            Kind::Addiu => Some(self.const_at(i.rs(), a, depth + 1)?.wrapping_add(i.simm())),
            Kind::Ori => Some(self.const_at(i.rs(), a, depth + 1)? | i.imm()),
            _ => None,
        }
    }

    /// Resolves the `jr` at `pc` (not through `ra`).
    fn jump_reg(self, pc: u32) -> JumpReg {
        let rs = self.at(pc).map_or(0, |(i, _)| i.rs());
        match self.switch_table(rs, pc) {
            Ok(t) => t,
            Err(why) => match (self.const_of(rs, pc), self.jump_list(pc)) {
                (Some(c), _) => JumpReg::Const(c),
                (None, Some(targets)) => JumpReg::Table { table: pc.wrapping_add(8), targets, bounded: false },
                (None, None) => JumpReg::Unresolved(why),
            },
        }
    }

    /// The `j` into the text, each with its delay slot, that follow the `jr` at `pc` and its slot,
    /// two at least: a table of jumps the register indexes by eight bytes (Spyro 2's
    /// `sll at,at,3 / addu at,at,a0 / jr at` at 8002BDF0, into eight `j` and `nop`).
    fn jump_list(self, pc: u32) -> Option<Vec<u32>> {
        let mut out = Vec::new();
        let mut a = pc.wrapping_add(8);
        while let (Some(Flow::Goto(t)), Some(_)) = (self.flow_at(a).filter(|_| self.at(a).is_some_and(|(_, k)| k == Kind::J)), self.at(a.wrapping_add(4))) {
            if !self.0.contains(t) {
                break;
            }
            out.push(a);
            a = a.wrapping_add(8);
        }
        (out.len() >= 2).then_some(out)
    }

    /// `sltiu at,idx,N / … / sll t,idx,2 / lui b,hi / addu b,b,t / lw r,lo(b) / jr r`.
    fn switch_table(self, rs: usize, pc: u32) -> Result<JumpReg, &'static str> {
        let d = self.find_def(rs, pc).ok_or("no definition of the jump register")?;
        let (lw, k) = self.at(d).ok_or("bad definition")?;
        if k != Kind::Lw {
            return Err("jump register not loaded from memory");
        }
        let d2 = self.find_def(lw.rs(), d).ok_or("no definition of the load base")?;
        let (add, k) = self.at(d2).ok_or("bad definition")?;
        if k != Kind::Addu {
            return Err("load base is not an addu");
        }
        let (mut base, mut index) = (None, None);
        for r in [add.rs(), add.rt()] {
            if base.is_none()
                && let Some(c) = self.const_of(r, d2)
            {
                base = Some(c);
                continue;
            }
            if let Some(s) = self.find_def(r, d2)
                && let Some((sll, Kind::Sll)) = self.at(s)
                && sll.sa() == 2
            {
                index = Some((sll.rt(), s));
            }
        }
        let (Some(base), Some((idx, sll_at))) = (base, index) else {
            return Err("addu operands are not table + index*4");
        };
        let table = base.wrapping_add(lw.simm());
        if self.data(table).is_none() {
            return Err("table outside the text and the far data");
        }
        let mut bound = None;
        let mut a = sll_at.wrapping_sub(4);
        for _ in 0..32 {
            match self.at(a) {
                Some((i, Kind::Sltiu)) if i.rs() == idx => {
                    bound = Some(i.imm());
                    break;
                }
                None if !self.0.contains(a) => break,
                _ => {}
            }
            a = a.wrapping_sub(4);
        }
        let mut targets = Vec::new();
        match bound {
            Some(n) => {
                for e in 0..n {
                    match self.data(table.wrapping_add(4 * e)) {
                        Some(t) => targets.push(t),
                        None => break,
                    }
                }
            }
            None => {
                for e in 0..512 {
                    match self.data(table.wrapping_add(4 * e)) {
                        Some(t) if self.at(t).is_some() => targets.push(t),
                        _ => break,
                    }
                }
            }
        }
        Ok(JumpReg::Table { table, targets, bounded: bound.is_some() })
    }
}

impl Discovery {
    /// Discovers the code of `exe`.
    pub fn run(exe: &PsxExe) -> Self {
        Self::run_far(exe, &[])
    }

    /// Discovers the code of `exe`, reading switch tables outside its text from `far` (the RAM
    /// the code was captured from). A table only names the entries to translate: the jump goes
    /// through the dispatch table, so a stale or wrong word costs code, never a wrong jump.
    pub fn run_far(exe: &PsxExe, far: Far) -> Self {
        Self::run_seeded(exe, far, &[])
    }

    /// [`Self::run_far`], with `seeds` called from outside the image (`jal` of another image into
    /// this text): each one not reached otherwise is an entry if every path from it decodes, as
    /// for a pointer (another overlay in the same slot may hold data there).
    pub fn run_seeded(exe: &PsxExe, far: Far, seeds: &[u32]) -> Self {
        Self::run_file(exe, far, &[], seeds)
    }

    /// [`Self::run_seeded`] for a text cut from a disc file, `file` at the address of its first
    /// byte: switch tables are read there too, and its tables of pointers into the text name
    /// functions as the text's own do (Silent Hill's body keeps its switch and handler tables
    /// in the 34 KB of data before its code, 621 words of code run in the first minute).
    pub fn run_file(exe: &PsxExe, far: Far, file: Far, seeds: &[u32]) -> Self {
        let around: Vec<(u32, &[u8])> = far.iter().chain(file).copied().collect();
        let img = Image(exe, &around);
        let mut d = Discovery::default();
        d.entries.insert(exe.pc0, Origin::Entry);
        d.explore(img, vec![exe.pc0]);
        for &s in seeds {
            if exe.contains(s) && s & 3 == 0 && !d.code.contains(&s) && d.clean_from(img, s) {
                d.entries.entry(s).or_insert(Origin::Call);
                d.explore(img, vec![s]);
            }
        }
        let mut tried = HashSet::new();
        let mut pointed: HashSet<u32> = (exe.text_addr..exe.text_end())
            .step_by(4)
            .filter_map(|a| exe.word(a))
            .chain(file.iter().flat_map(|&(_, b)| words(b)))
            .filter(|&w| w & 3 == 0 && exe.contains(w))
            .collect();
        loop {
            pointed.extend(d.built_pointers(img));
            let tabled = tabled(exe, file, &d.entries);
            let mut changed = false;
            for a in (exe.text_addr..exe.text_end()).step_by(4) {
                let w = exe.word(a).unwrap_or(0);
                let prologue = matches!(w >> 16, 0x27BD | 0x23BD) && w & 0x8000 != 0;
                let pointer = pointed.contains(&a) && (after_end(img, a) || tabled.contains(&a));
                // Tried again once it becomes a pointer: a leaf test may have failed before the code
                // building it was found.
                if d.code.contains(&a) || !(prologue || pointer || after_return(exe, a)) || !tried.insert((a, pointer)) {
                    continue;
                }
                if !prologue {
                    if after_return(exe, a) && leaf_shape(img, a) {
                        d.entries.entry(a).or_insert(Origin::AfterReturn);
                        d.explore(img, vec![a]);
                        changed = true;
                    } else if pointer && d.clean_from(img, a) {
                        d.entries.entry(a).or_insert(Origin::Pointer);
                        d.explore(img, vec![a]);
                        changed = true;
                    }
                    continue;
                }
                let mut s = a;
                for _ in 0..8 {
                    let p = s.wrapping_sub(4);
                    if !exe.contains(p) || d.code.contains(&p) || exe.word(p) == Some(0) {
                        break;
                    }
                    if exe.word(p.wrapping_sub(4)) == Some(JR_RA) || img.flow_at(p) != Some(Flow::Next) {
                        break;
                    }
                    s = p;
                }
                d.entries.entry(s).or_insert(Origin::Prologue);
                d.explore(img, vec![s]);
                changed = true;
            }
            if !changed {
                break;
            }
        }
        d.visited = HashSet::new();
        d
    }

    fn explore(&mut self, img: Image, mut work: Vec<u32>) {
        let exe = img.0;
        while let Some(mut pc) = work.pop() {
            loop {
                if self.visited.contains(&pc) {
                    break;
                }
                let Some((i, k)) = img.at(pc) else {
                    let why = exe.word(pc).map_or(DeadEnd::Outside, DeadEnd::Invalid);
                    self.dead_ends.insert(pc, why);
                    break;
                };
                self.visited.insert(pc);
                self.code.insert(pc);
                let f = flow(i, k, pc);
                if f == Flow::Next {
                    pc = pc.wrapping_add(4);
                    continue;
                }
                let slot = pc.wrapping_add(4);
                let after = pc.wrapping_add(8);
                if img.at(slot).is_some() {
                    self.code.insert(slot);
                    self.delay_slots.insert(slot, pc);
                }
                match f {
                    Flow::Branch(t) => work.extend([t, after]),
                    Flow::Goto(t) => work.push(t),
                    Flow::Call(t) => {
                        self.call(exe, t, pc, &mut work);
                        work.push(after);
                    }
                    Flow::Return => {}
                    Flow::JumpReg => {
                        let j = img.jump_reg(pc);
                        match &j {
                            JumpReg::Table { targets, .. } => work.extend(targets),
                            JumpReg::Const(c) if exe.contains(*c) => work.push(*c),
                            _ => {}
                        }
                        self.jumps.insert(pc, j);
                    }
                    Flow::CallReg => {
                        let c = img.const_of(i.rs(), pc);
                        self.indirect_calls.insert(pc, c);
                        if let Some(c) = c {
                            self.call(exe, c, pc, &mut work);
                        }
                        work.push(after);
                    }
                    Flow::Next => unreachable!(),
                }
                break;
            }
        }
    }

    /// Text addresses the discovered code builds with `lui` and `addiu`/`ori` (a handler stored
    /// in a state variable, a callback passed as an argument).
    fn built_pointers(&self, img: Image) -> Vec<u32> {
        self.code
            .iter()
            .filter_map(|&a| match img.at(a)? {
                (i, Kind::Addiu | Kind::Ori) if i.rs() != 0 => img.const_of(i.rt(), a.wrapping_add(4)),
                _ => None,
            })
            .filter(|&c| c & 3 == 0 && img.0.contains(c))
            .collect()
    }

    /// Every path the descent would take from `a` decodes and stays in the text.
    fn clean_from(&self, img: Image, a: u32) -> bool {
        let mut trial = Discovery { visited: self.visited.clone(), ..Discovery::default() };
        trial.explore(img, vec![a]);
        trial.dead_ends.is_empty()
    }

    fn call(&mut self, exe: &PsxExe, target: u32, site: u32, work: &mut Vec<u32>) {
        self.calls.entry(target).or_default().push(site);
        if exe.contains(target) {
            self.entries.entry(target).or_insert(Origin::Call);
            work.push(target);
        }
    }

    /// The body of the function at `entry`.
    pub fn body(&self, exe: &PsxExe, entry: u32) -> Body {
        let img = Image(exe, &[]);
        let other = |t: u32| t != entry && self.entries.contains_key(&t);
        let mut body = Body::default();
        let mut seen = HashSet::new();
        let mut work = vec![entry];
        while let Some(mut pc) = work.pop() {
            loop {
                if !seen.insert(pc) {
                    break;
                }
                let Some((i, k)) = img.at(pc) else { break };
                body.insns.insert(pc);
                let f = flow(i, k, pc);
                if f == Flow::Next {
                    pc = pc.wrapping_add(4);
                    continue;
                }
                if img.at(pc.wrapping_add(4)).is_some() {
                    body.insns.insert(pc.wrapping_add(4));
                }
                let after = pc.wrapping_add(8);
                match f {
                    Flow::Branch(t) => work.extend([t, after]),
                    Flow::Goto(t) if exe.contains(t) && !other(t) => work.push(t),
                    Flow::Call(_) | Flow::CallReg => work.push(after),
                    Flow::Return => body.returns.push(pc),
                    Flow::JumpReg => {
                        if let Some(JumpReg::Table { targets, .. }) = self.jumps.get(&pc) {
                            work.extend(targets.iter().filter(|&&t| exe.contains(t) && !other(t)));
                        }
                    }
                    _ => {}
                }
                break;
            }
        }
        body
    }
}

//! Load-delay hazards: places where code reads a register in the load delay slot of the load
//! that writes it, and so depends on the R3000A seeing the old value.
//!
//! The translator emits loads that complete at once (the Allegrex has no visible load delay).
//! That is only exact if no reachable instruction reads a loaded register in the slot right
//! after the load. This module finds the ones that do:
//!
//! * a load (or `mfc0`/`mfc2`/`cfc2`) followed by an instruction that reads its target, except
//!   `lwl`+`lwr` of the same register (they merge as intended on both CPUs);
//! * a load in a branch delay slot whose successor (fall-through and target, the callee, or for
//!   `jr ra` the return sites of every caller of the function) reads its target;
//! * a load in the delay slot of a register jump or call that cannot be resolved statically;
//! * `lwc2` followed by a GTE command, or an `mfc2`/`swc2` of the same GTE register.

use std::collections::{BTreeMap, HashMap};

use crate::discover::{Discovery, Flow, Image, JumpReg};
use crate::exe::PsxExe;
use crate::insn::{Kind, Uses};

/// What makes a site a hazard.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HazardKind {
    /// The next instruction reads the loaded register.
    Sequential,
    /// The load is in a branch, jump or call delay slot and a successor reads the register.
    BranchSlot,
    /// The load is in a `jr ra` delay slot and a return site reads the register.
    ReturnSlot,
    /// The load is in the delay slot of a register jump or call with unknown targets.
    UnknownSlot,
    /// `lwc2` followed by a GTE command or an access to the same GTE register.
    Cop2,
}

/// The load-delay analysis of one executable.
#[derive(Clone, Debug, Default)]
pub struct Hazards {
    /// Load address -> why its delay slot matters.
    pub sites: BTreeMap<u32, HazardKind>,
    /// Delayed loads checked (loads and COP moves with a nonzero target).
    pub loads: usize,
    /// `lwl`/`lwr` pairs of the same register (benign).
    pub merged_pairs: usize,
    /// Loads in a `jr ra` delay slot of a function nobody calls directly (nothing to check).
    pub return_slot_no_caller: usize,
}

impl Hazards {
    pub fn check(exe: &PsxExe, d: &Discovery) -> Self {
        let img = Image(exe, &[], false);
        let mut returns: HashMap<u32, Vec<u32>> = HashMap::new();
        for &f in d.entries.keys() {
            for r in d.body(exe, f).returns {
                returns.entry(r).or_default().push(f);
            }
        }
        let reads = |a: u32, r: usize| {
            d.code.contains(&a) && img.at(a).is_some_and(|(i, k)| Uses::of(i, k).read & (1 << r) != 0)
        };
        let mut h = Hazards::default();
        for &a in &d.code {
            let Some((i, k)) = img.at(a) else { continue };
            let slot_of = d.delay_slots.get(&a).copied();
            if k == Kind::Lwc2 {
                if slot_of.is_none() && d.code.contains(&(a + 4)) {
                    let cop2 = match img.at(a + 4) {
                        Some((_, Kind::Gte)) => true,
                        Some((n, Kind::Mfc2)) => n.rd() == i.rt(),
                        Some((n, Kind::Swc2)) => n.rt() == i.rt(),
                        _ => false,
                    };
                    if cop2 {
                        h.sites.insert(a, HazardKind::Cop2);
                    }
                }
                continue;
            }
            let r = i.rt();
            if !k.is_delayed_load() || r == 0 {
                continue;
            }
            h.loads += 1;
            let Some(b) = slot_of else {
                if reads(a + 4, r) {
                    match img.at(a + 4) {
                        Some((n, Kind::Lwl | Kind::Lwr))
                            if matches!(k, Kind::Lwl | Kind::Lwr) && n.rt() == r =>
                        {
                            h.merged_pairs += 1
                        }
                        _ => {
                            h.sites.insert(a, HazardKind::Sequential);
                        }
                    }
                }
                continue;
            };
            let (bi, bk) = img.at(b).expect("a delay slot follows a decoded branch");
            let after = b + 8;
            let (succ, kind): (Vec<u32>, _) = match crate::discover::flow(bi, bk, b) {
                Flow::Branch(t) => (vec![after, t], HazardKind::BranchSlot),
                Flow::Call(t) if matches!(bk, Kind::Bltzal | Kind::Bgezal) && bi.rs() != 0 => {
                    (vec![after, t], HazardKind::BranchSlot)
                }
                Flow::Goto(t) | Flow::Call(t) => (vec![t], HazardKind::BranchSlot),
                Flow::Return => {
                    let fs = returns.get(&b).map_or(&[][..], Vec::as_slice);
                    let callers: Vec<u32> = fs
                        .iter()
                        .flat_map(|f| d.calls.get(f).into_iter().flatten().map(|s| s + 8))
                        .collect();
                    if callers.is_empty() {
                        h.return_slot_no_caller += 1;
                    }
                    (callers, HazardKind::ReturnSlot)
                }
                Flow::JumpReg => match d.jumps.get(&b) {
                    Some(JumpReg::Table { targets, .. }) => (targets.clone(), HazardKind::BranchSlot),
                    Some(JumpReg::Const(c)) if exe.contains(*c) => (vec![*c], HazardKind::BranchSlot),
                    _ => {
                        h.sites.insert(a, HazardKind::UnknownSlot);
                        continue;
                    }
                },
                Flow::CallReg => match d.indirect_calls.get(&b) {
                    Some(Some(c)) if exe.contains(*c) => (vec![*c], HazardKind::BranchSlot),
                    _ => {
                        h.sites.insert(a, HazardKind::UnknownSlot);
                        continue;
                    }
                },
                Flow::Next => unreachable!("delay slot of a non-branch"),
            };
            if succ.into_iter().any(|s| reads(s, r)) {
                h.sites.insert(a, kind);
            }
        }
        h
    }
}

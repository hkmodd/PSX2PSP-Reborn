//! Code discovery and the load-delay hazard check.

use std::collections::BTreeMap;

use psx2psp_recomp::{DeadEnd, Discovery, HazardKind, Hazards, JumpReg, Origin, PsxExe};

const BASE: u32 = 0x8001_0000;
const ZERO: u32 = 0;
const AT: u32 = 1;
const V0: u32 = 2;
const V1: u32 = 3;
const A0: u32 = 4;
const A1: u32 = 5;
const T0: u32 = 8;
const T1: u32 = 9;
const SP: u32 = 29;
const RA: u32 = 31;
const NOP: u32 = 0;

const fn r(funct: u32, rs: u32, rt: u32, rd: u32, sa: u32) -> u32 {
    (rs << 21) | (rt << 16) | (rd << 11) | (sa << 6) | funct
}
const fn i(op: u32, rs: u32, rt: u32, imm: u16) -> u32 {
    (op << 26) | (rs << 21) | (rt << 16) | imm as u32
}
const fn at(n: u32) -> u32 {
    BASE + 4 * n
}
const fn j(op: u32, n: u32) -> u32 {
    (op << 26) | ((at(n) >> 2) & 0x03FF_FFFF)
}

/// A PS-X EXE whose text is `words` at `BASE`, entry at `BASE`.
fn exe(words: &[u32]) -> PsxExe {
    let mut f = vec![0u8; 0x800];
    f[..8].copy_from_slice(b"PS-X EXE");
    for (o, v) in [(0x10, BASE), (0x18, BASE), (0x1C, 4 * words.len() as u32)] {
        f[o..o + 4].copy_from_slice(&v.to_le_bytes());
    }
    f.extend(words.iter().flat_map(|w| w.to_le_bytes()));
    PsxExe::parse(&f).unwrap()
}

/// `main` calls `f` and switches on `a1` through a two-entry table; a third function is only
/// reachable through its prologue. Two load-delay hazards: a sequential one in `main` and one in
/// the `jr ra` delay slot of `f`, read at the return site in `main`.
fn program() -> PsxExe {
    let mut w = vec![NOP; 39];
    let code = [
        i(9, SP, SP, (-8i16) as u16),     // 0  main: addiu sp,sp,-8
        j(3, 17),                         // 1  jal f
        NOP,                              // 2
        r(0x21, A0, ZERO, A1, 0),         // 3  addu a1,a0,zero  <- reads f's delay-slot load
        i(0x23, SP, V0, 0),               // 4  lw v0,0(sp)
        r(0x21, V0, V0, V1, 0),           // 5  addu v1,v0,v0    <- reads it in the delay slot
        i(0xB, A1, AT, 2),                // 6  sltiu at,a1,2
        i(4, AT, ZERO, 7),                // 7  beq at,zero,15
        r(0, 0, A1, T0, 2),               // 8  sll t0,a1,2
        i(0xF, 0, T1, 0x8001),            // 9  lui t1,0x8001
        r(0x21, T1, T0, T1, 0),           // 10 addu t1,t1,t0
        i(0x23, T1, T0, 0x80),            // 11 lw t0,0x80(t1)
        NOP,                              // 12
        r(8, T0, 0, 0, 0),                // 13 jr t0
        NOP,                              // 14
        r(8, RA, 0, 0, 0),                // 15 jr ra
        NOP,                              // 16
        r(8, RA, 0, 0, 0),                // 17 f: jr ra
        i(0x23, SP, A0, 0),               // 18 lw a0,0(sp)
        j(2, 15),                         // 19 case 0: j 15
        NOP,                              // 20
        j(2, 15),                         // 21 case 1: j 15
        NOP,                              // 22
    ];
    w[..code.len()].copy_from_slice(&code);
    w[32] = at(19);
    w[33] = at(21);
    w[36] = i(9, SP, SP, (-16i16) as u16); // 36 g: addiu sp,sp,-16
    w[37] = r(8, RA, 0, 0, 0); //              37 jr ra
    w[38] = i(9, SP, SP, 16); //               38 addiu sp,sp,16
    exe(&w)
}

#[test]
fn discovery_finds_calls_switch_tables_and_prologues() {
    let x = program();
    let d = Discovery::run(&x);
    let entries: Vec<_> = d.entries.iter().map(|(&a, &o)| (a, o)).collect();
    assert_eq!(entries, [(at(0), Origin::Entry), (at(17), Origin::Call), (at(36), Origin::Prologue)]);
    assert_eq!(d.calls[&at(17)], [at(1)]);
    assert_eq!(
        d.jumps[&at(13)],
        JumpReg::Table { table: at(32), targets: vec![at(19), at(21)], bounded: true }
    );
    assert_eq!(d.delay_slots[&at(18)], at(17));
    assert_eq!(d.delay_slots[&at(8)], at(7));
    assert!(d.dead_ends.is_empty());
    let code: Vec<u32> = (0..=22).chain(36..=38).map(at).collect();
    assert_eq!(d.code.iter().copied().collect::<Vec<_>>(), code);

    let main = d.body(&x, at(0));
    assert_eq!(main.returns, [at(15)]);
    assert_eq!(main.insns.len(), 21);
    assert!(!main.insns.contains(&at(17)));
    assert_eq!(d.body(&x, at(17)).returns, [at(17)]);
}

/// A switch table outside the text (in data the code was captured with) is read from the far
/// data; without it the jump stays unresolved.
#[test]
fn switch_tables_outside_the_text_are_read_from_far_data() {
    let mut x = program();
    x.text[4 * 11..4 * 12].copy_from_slice(&i(0x23, T1, T0, 0x1000).to_le_bytes()); // lw t0,0x1000(t1)
    let table = BASE + 0x1000;
    assert!(matches!(Discovery::run(&x).jumps[&at(13)], JumpReg::Unresolved(_)));
    let data: Vec<u8> = [at(19), at(21), 0x1234_5678].iter().flat_map(|w| w.to_le_bytes()).collect();
    let d = Discovery::run_far(&x, &[(table, &data)]);
    assert_eq!(d.jumps[&at(13)], JumpReg::Table { table, targets: vec![at(19), at(21)], bounded: true });
    assert!(d.code.contains(&at(19)) && d.code.contains(&at(21)));
}

/// A leaf that nothing in the image calls, after a `jr` that is not `jr ra` (Silent Hill's SetSp
/// after the BIOS stubs), is found only as a seed called from another image; a seed whose code
/// does not decode is not taken.
#[test]
fn seeds_called_from_another_image_are_entries() {
    const T2: u32 = 10;
    let x = exe(&[
        r(8, RA, 0, 0, 0),          // 0 jr ra
        NOP,                        // 1
        i(9, ZERO, T2, 0xB0),       // 2 addiu t2,zero,0xb0
        r(8, T2, 0, 0, 0),          // 3 jr t2
        i(9, ZERO, T1, 9),          // 4 addiu t1,zero,9
        NOP,                        // 5
        r(0x21, SP, ZERO, V0, 0),   // 6 SetSp: addu v0,sp,zero
        r(8, RA, 0, 0, 0),          // 7 jr ra
        r(0x21, A0, ZERO, SP, 0),   // 8 addu sp,a0,zero
        r(2, 1, A0, A0, 8),         // 9 rotr (Allegrex): not code
    ]);
    assert!(!Discovery::run(&x).code.contains(&at(6)));
    let d = Discovery::run_seeded(&x, &[], &[at(6), at(9)]);
    assert_eq!(d.entries.get(&at(6)), Some(&Origin::Call));
    assert!(d.code.contains(&at(8)));
    assert!(!d.entries.contains_key(&at(9)) && !d.code.contains(&at(9)));
}

/// A handler that ends in a tail call, reached only through a pointer in data, is found; a
/// pointer to a word that does not decode is not followed.
#[test]
fn discovery_follows_pointers_to_handlers() {
    let x = exe(&[
        r(8, RA, 0, 0, 0),         // 0 jr ra
        NOP,                       // 1
        i(9, ZERO, V0, 15),        // 2 h: addiu v0,zero,15
        j(2, 0),                   // 3 j 0: a tail call, no jr ra of its own
        NOP,                       // 4
        at(2),                     // 5 pointer to h
        at(9),                     // 6 pointer to 9
        j(2, 0),                   // 7 j 0
        NOP,                       // 8
        r(2, 1, A0, A0, 8),        // 9 rotr (Allegrex): not code
    ]);
    let d = Discovery::run(&x);
    let entries: Vec<_> = d.entries.iter().map(|(&a, &o)| (a, o)).collect();
    assert_eq!(entries, [(at(0), Origin::Entry), (at(2), Origin::Pointer)]);
    assert_eq!(d.code.iter().copied().collect::<Vec<_>>(), (0..=4).map(at).collect::<Vec<_>>());
    assert!(d.dead_ends.is_empty());
}

/// A handler whose address the code builds (`lui`/`addiu`, stored as the next state) is found.
#[test]
fn discovery_follows_pointers_built_in_code() {
    let x = exe(&[
        i(0xF, ZERO, A1, (at(4) >> 16) as u16), // 0 lui a1,hi(h)
        i(9, A1, A1, at(4) as u16),              // 1 addiu a1,a1,lo(h)
        r(8, RA, 0, 0, 0),                       // 2 jr ra
        i(0x2B, ZERO, A1, 0x100),                // 3 sw a1,0x100(zero)
        i(9, ZERO, V0, 1),                       // 4 h: addiu v0,zero,1
        j(2, 0),                                 // 5 j 0
        NOP,                                     // 6
    ]);
    let d = Discovery::run(&x);
    assert_eq!(d.entries.get(&at(4)), Some(&Origin::Pointer));
    assert_eq!(d.code.len(), 7);
    assert!(d.dead_ends.is_empty());
}

/// A callback whose `addiu` is the delay slot of the call it is passed to is found (Spider-Man's
/// `lui a2 / jal 8002DADC / addiu a2,a2,0x1E90` at 8002DC40).
#[test]
fn a_pointer_built_in_a_delay_slot_is_found() {
    let x = exe(&[
        i(0xF, ZERO, 6, (at(7) >> 16) as u16), // 0 lui a2,hi(h)
        j(3, 5),                                // 1 jal 5
        i(9, 6, 6, at(7) as u16),               // 2 addiu a2,a2,lo(h)
        r(8, RA, 0, 0, 0),                      // 3 jr ra
        i(0x2B, ZERO, A1, 0x100),               // 4 sw a1,0x100(zero)
        r(8, RA, 0, 0, 0),                      // 5 jr ra
        i(0x2B, ZERO, A1, 0x104),               // 6 sw a1,0x104(zero)
        i(9, ZERO, V0, 1),                      // 7 h: addiu v0,zero,1
        j(2, 0),                                // 8 j 0
        NOP,                                    // 9
    ]);
    let d = Discovery::run(&x);
    assert_eq!(d.entries.get(&at(7)), Some(&Origin::Pointer));
    assert!([7, 8, 9].iter().all(|&n| d.code.contains(&at(n))));
}


/// A word after a return that fails the leaf test is tried again as a pointer once the code
/// building its address is found (Crash 2's rasterizer 8003E794, `lui`/`addiu` in a function
/// found later by its prologue).
#[test]
fn a_failed_leaf_is_tried_again_as_a_built_pointer() {
    let x = exe(&[
        r(8, RA, 0, 0, 0),                       // 0 jr ra
        NOP,                                     // 1
        i(9, ZERO, V0, 1),                       // 2 h: addiu v0,zero,1
        j(2, 0),                                 // 3 j 0: no jr ra, not a leaf
        NOP,                                     // 4
        r(2, 1, A0, A0, 8),                      // 5 rotr (Allegrex): not code
        i(9, SP, SP, (-8i16) as u16),            // 6 g: addiu sp,sp,-8
        i(0xF, ZERO, A1, (at(2) >> 16) as u16),  // 7 lui a1,hi(h)
        i(9, A1, A1, at(2) as u16),              // 8 addiu a1,a1,lo(h)
        i(0x2B, ZERO, A1, 0x100),                // 9 sw a1,0x100(zero)
        r(8, RA, 0, 0, 0),                       // 10 jr ra
        i(9, SP, SP, 8),                         // 11 addiu sp,sp,8
    ]);
    let d = Discovery::run(&x);
    assert_eq!(d.entries.get(&at(6)), Some(&Origin::Prologue));
    assert_eq!(d.entries.get(&at(2)), Some(&Origin::Pointer));
    assert!(!d.code.contains(&at(5)));
    assert!(d.dead_ends.is_empty());
}
/// A pointer to code after a long run of zero words (Crash 2's rasterizer 800409F4, 203 zero
/// words after a `b`) is a handler; the zeros are not code.
#[test]
fn a_pointer_after_a_long_zero_gap_is_a_handler() {
    let mut w = vec![
        r(8, RA, 0, 0, 0),   // 0 jr ra
        NOP,                 // 1
        j(2, 0),             // 2 j 0
        NOP,                 // 3
    ];
    w.extend([0; 40]);
    let h = w.len() as u32;
    w.extend([i(9, ZERO, V0, 1), j(2, 0), NOP, at(h)]); // h: addiu v0,zero,1; j 0; pointer to h
    let d = Discovery::run(&exe(&w));
    assert_eq!(d.entries.get(&at(h)), Some(&Origin::Pointer));
    assert!(!d.code.contains(&at(10)));
}

/// A function after data words, reached only through a table of text pointers that also holds a
/// function already found, is a function too (Sheep Dog's constructor list at 8008C9AC: its
/// first member 8008C868 follows data, the others follow returns); the data words are not code.
#[test]
fn a_pointer_in_a_table_beside_a_known_function_is_a_function() {
    let x = exe(&[
        r(8, RA, 0, 0, 0),  // 0 jr ra
        NOP,                // 1
        r(2, 1, A0, A0, 8), // 2 data (rotr, Allegrex: not code)
        0x0001_0000,        // 3 data
        i(9, ZERO, V0, 1),  // 4 f: addiu v0,zero,1 (after data: no return before it)
        r(8, RA, 0, 0, 0),  // 5 jr ra
        NOP,                // 6
        i(9, ZERO, V0, 2),  // 7 g: addiu v0,zero,2 (after a return: found as a leaf)
        r(8, RA, 0, 0, 0),  // 8 jr ra
        NOP,                // 9
        at(4),              // 10 table: f
        at(7),              // 11 g
        0,                  // 12 end
    ]);
    let d = Discovery::run(&x);
    assert_eq!(d.entries.get(&at(7)), Some(&Origin::AfterReturn));
    assert_eq!(d.entries.get(&at(4)), Some(&Origin::Pointer));
    assert!(!d.code.contains(&at(2)) && !d.code.contains(&at(3)));
    assert!(d.dead_ends.is_empty());
}

/// A lone pointer to code after data words is not followed: no return before it, no known
/// function beside it in a table.
#[test]
fn a_lone_pointer_after_data_is_not_followed() {
    let x = exe(&[
        r(8, RA, 0, 0, 0),  // 0 jr ra
        NOP,                // 1
        r(2, 1, A0, A0, 8), // 2 data (rotr, Allegrex: not code)
        0x0001_0000,        // 3 data
        i(9, ZERO, V0, 1),  // 4 addiu v0,zero,1
        r(8, RA, 0, 0, 0),  // 5 jr ra
        NOP,                // 6
        0x0001_0000,        // 7 data
        at(4),              // 8 pointer to 4
        0,                  // 9
    ]);
    let d = Discovery::run(&x);
    assert!(!d.code.contains(&at(4)));
}

#[test]
fn hazards_in_sequence_and_across_returns() {
    let x = program();
    let h = Hazards::check(&x, &Discovery::run(&x));
    let want = BTreeMap::from([(at(4), HazardKind::Sequential), (at(18), HazardKind::ReturnSlot)]);
    assert_eq!(h.sites, want);
    assert_eq!((h.loads, h.merged_pairs, h.return_slot_no_caller), (3, 0, 0));
}

#[test]
fn unknown_jumps_and_invalid_words_are_reported() {
    let x = exe(&[
        r(9, T0, 0, RA, 0),   // jalr t0 (t0 unknown)
        i(0x23, SP, A0, 0),   // lw a0,0(sp) in its delay slot
        i(0x22, SP, A0, 3),   // lwl a0,3(sp)
        i(0x26, SP, A0, 0),   // lwr a0,0(sp): merges with the lwl
        r(2, 1, A0, A0, 8),   // rotr (Allegrex), not a canonical R3000A srl
    ]);
    let d = Discovery::run(&x);
    assert_eq!(d.indirect_calls[&at(0)], None);
    assert_eq!(d.dead_ends[&at(4)], DeadEnd::Invalid(r(2, 1, A0, A0, 8)));
    let h = Hazards::check(&x, &d);
    assert_eq!(h.sites, BTreeMap::from([(at(1), HazardKind::UnknownSlot)]));
    assert_eq!(h.merged_pairs, 1);
}

/// A `jr` into the `j` right after it (a table of jumps indexed by eight bytes) reaches each
/// of them and their targets; one `j` alone is no table.
#[test]
fn a_table_of_jumps_after_a_jump_register_is_followed() {
    let x = exe(&[
        r(0, 0, AT, AT, 3),   // 0 sll at,at,3
        r(0x21, AT, A0, AT, 0), // 1 addu at,at,a0
        r(8, AT, 0, 0, 0),    // 2 jr at
        NOP,                  // 3
        j(2, 10),             // 4 j 10
        NOP,                  // 5
        i(1, ZERO, 1, 5),     // 6 b 12 (bgez zero)
        NOP,                  // 7
        r(8, RA, 0, 0, 0),    // 8 jr ra (not reached)
        NOP,                  // 9
        r(8, RA, 0, 0, 0),    // 10 jr ra
        NOP,                  // 11
        r(8, RA, 0, 0, 0),    // 12 jr ra
        NOP,                  // 13
    ]);
    let d = Discovery::run(&x);
    assert_eq!(d.jumps[&at(2)], JumpReg::Table { table: at(4), targets: vec![at(4), at(6)], bounded: false });
    assert!([4, 5, 6, 7, 10, 11, 12, 13].iter().all(|&n| d.code.contains(&at(n))));
    let one = exe(&[r(8, AT, 0, 0, 0), NOP, j(2, 4), NOP, r(8, RA, 0, 0, 0), NOP]);
    assert!(matches!(Discovery::run(&one).jumps[&at(0)], JumpReg::Unresolved(_)));
}

/// Blocks of `1 << s` bytes after the `jr`, indexed by `x << s` (Crash 2's copy tails at
/// 8003BB14, 32 bytes each), are followed up to the first block that does not end in a jump.
#[test]
fn a_table_of_blocks_after_a_jump_register_is_followed() {
    let mut w = vec![
        i(0xF, ZERO, T0, (at(6) >> 16) as u16), // 0 lui t0,hi(tails)
        i(9, T0, T0, at(6) as u16),              // 1 addiu t0,t0,lo(tails)
        r(0, 0, 6, V0, 5),                       // 2 sll v0,a2,5
        r(0x21, T0, V0, T0, 0),                  // 3 addu t0,t0,v0
        r(8, T0, 0, 0, 0),                       // 4 jr t0
        NOP,                                     // 5
        r(8, RA, 0, 0, 0),                       // 6 tails: jr ra
    ];
    w.resize(14, NOP);
    w.extend([i(9, ZERO, V0, 1), r(8, RA, 0, 0, 0)]); // 14 addiu v0,zero,1 / 15 jr ra
    w.resize(30, NOP);
    let d = Discovery::run(&exe(&w));
    assert_eq!(d.jumps[&at(4)], JumpReg::Table { table: at(6), targets: vec![at(6), at(14)], bounded: false });
    assert!([6, 7, 14, 15, 16].iter().all(|&n| d.code.contains(&at(n))));
    assert!(!d.code.contains(&at(22)));
}

/// Runs discovery and the hazard check on real executables:
/// `PSX2PSP_EXES=<dir> cargo test -p psx2psp-recomp --test discover -- --ignored --nocapture`.
#[test]
#[ignore = "needs PSX2PSP_EXES"]
fn real_executables_have_no_hazards() {
    let dir = std::env::var("PSX2PSP_EXES").expect("PSX2PSP_EXES");
    let mut paths: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
    paths.sort();
    println!(
        "exe | funcs (desc, prologue) | code bytes | code% | invalid stops | jt (bounded/unbounded) | \
         jt entries | jr const | jr unresolved | jalr | jalr const | loads | lwl/lwr pairs | \
         jr-ra slot no caller | hazards"
    );
    let (mut funcs, mut code, mut text, mut bad) = (0, 0, 0, 0);
    for path in &paths {
        let x = PsxExe::parse(&std::fs::read(path).unwrap()).unwrap();
        let d = Discovery::run(&x);
        let h = Hazards::check(&x, &d);
        let prol = d.entries.values().filter(|&&o| o == Origin::Prologue).count();
        let (mut jb, mut ju, mut je, mut jc, mut jn) = (0, 0, 0, 0, 0);
        for j in d.jumps.values() {
            match j {
                JumpReg::Table { targets, bounded, .. } => {
                    *if *bounded { &mut jb } else { &mut ju } += 1;
                    je += targets.len();
                }
                JumpReg::Const(_) => jc += 1,
                JumpReg::Unresolved(_) => jn += 1,
            }
        }
        let invalid: Vec<_> = d.dead_ends.iter().filter(|(_, e)| matches!(e, DeadEnd::Invalid(_))).collect();
        let jalr_const = d.indirect_calls.values().filter(|c| c.is_some()).count();
        println!(
            "{} | {} ({}, {prol}) | {} | {:.1} | {} | {jb}/{ju} | {je} | {jc} | {jn} | {} | {jalr_const} | {} | {} | {} | {:?}",
            path.file_name().unwrap().to_string_lossy(),
            d.entries.len(),
            d.entries.len() - prol,
            4 * d.code.len(),
            400.0 * d.code.len() as f64 / x.text.len() as f64,
            invalid.len(),
            d.indirect_calls.len(),
            h.loads,
            h.merged_pairs,
            h.return_slot_no_caller,
            h.sites,
        );
        for (a, e) in invalid.iter().take(4) {
            println!("   stop at {a:08x}: {e:x?}");
        }
        funcs += d.entries.len();
        code += 4 * d.code.len();
        text += x.text.len();
        bad += h.sites.len();
    }
    println!("TOTAL funcs {funcs} code {code} text {text} {:.1}%", 100.0 * code as f64 / text as f64);
    assert!(!paths.is_empty());
    assert_eq!(bad, 0, "load-delay hazards found");
}

/// The recipes (hooks.rs) found in each executable of `PSX2PSP_EXES`, by address.
/// `PSX2PSP_EXES=<dir> cargo test -p psx2psp-recomp --test discover -- --ignored --nocapture`.
#[test]
#[ignore = "needs PSX2PSP_EXES"]
fn real_executables_recipes() {
    let dir = std::env::var("PSX2PSP_EXES").expect("PSX2PSP_EXES");
    let mut paths: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
    paths.sort();
    for path in &paths {
        let Ok(x) = PsxExe::parse(&std::fs::read(path).unwrap()) else { continue };
        let d = Discovery::run(&x);
        let h = psx2psp_recomp::hooks::find(&x, &d);
        println!("{}: {:x?}", path.file_name().unwrap().to_string_lossy(), h);
    }
}
/// Load address, discovery and translated size of every file of `PSX2PSP_OVLS` taken as an
/// overlay (`overlay::locate_all`): `PSX2PSP_OVLS=<dir> cargo test -p psx2psp-recomp --release
/// --test discover real_overlays -- --ignored --nocapture`.
#[test]
#[ignore = "needs PSX2PSP_OVLS"]
fn real_overlays() {
    use psx2psp_recomp::overlay;
    let dir = std::env::var("PSX2PSP_OVLS").expect("PSX2PSP_OVLS");
    let mut paths: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
    paths.sort();
    println!("file | bytes | base | votes | funcs | code% | invalid stops | jr unresolved | hazards | host bytes");
    let (mut found, mut code, mut host_max) = (0, 0usize, BTreeMap::<u32, usize>::new());
    let files: Vec<Vec<u8>> = paths.iter().map(|p| std::fs::read(p).unwrap()).collect();
    let bases = overlay::locate_all(&files.iter().map(|f| f.as_slice()).collect::<Vec<_>>());
    for ((path, bytes), at) in paths.iter().zip(&files).zip(bases) {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let Some((base, votes)) = at else {
            println!("{name} | {} | - |", bytes.len());
            continue;
        };
        let x = overlay::as_exe(bytes, base).unwrap();
        let d = Discovery::run(&x);
        let h = Hazards::check(&x, &d);
        let invalid = d.dead_ends.values().filter(|e| matches!(e, DeadEnd::Invalid(_))).count();
        let unresolved = d.jumps.values().filter(|j| matches!(j, JumpReg::Unresolved(_))).count();
        let host = psx2psp_recomp::translate(&x, &d, &h, 0x0880_0000).map(|t| t.words.len() * 4).unwrap_or(0);
        println!(
            "{name} | {} | {base:08x} | {votes} | {} | {:.1} | {invalid} | {unresolved} | {} | {host}",
            bytes.len(),
            d.entries.len(),
            400.0 * d.code.len() as f64 / x.text.len() as f64,
            h.sites.len()
        );
        found += 1;
        code += 4 * d.code.len();
        let m = host_max.entry(base).or_default();
        *m = (*m).max(host);
    }
    println!("TOTAL {found} of {} located, {code} B of code; largest translation per base: {host_max:x?}", paths.len());
    assert!(!paths.is_empty());
}

/// Code after words that are not instructions, entered from outside the image: an interpreter's
/// op words, then native code that returns through `jalr s5,ra` (Crash 2's GOOL entries). What
/// follows the `jalr` through ra is not code by itself, nor a run too short to be an island.
#[test]
fn an_island_of_code_after_data_is_found() {
    const S0: u32 = 16;
    const S5: u32 = 21;
    const S6: u32 = 22;
    let w = [
        r(8, RA, 0, 0, 0),           // 0  entry: jr ra
        NOP,                         // 1
        0x1C30_184E,                 // 2  op words: not instructions
        0x49BE_0BE0,                 // 3
        NOP,                         // 4  entered at a nop
        i(0x23, S6, V0, 0xFFFC),     // 5  lw v0,-4(s6)
        i(9, S6, S6, 0xFFFC),        // 6  addiu s6,s6,-4
        i(0x2B, S0, V0, 372),        // 7  sw v0,372(s0)
        i(0x2B, S0, S6, 188),        // 8  sw s6,188(s0)
        r(9, RA, 0, S5, 0),          // 9  jalr s5,ra
        NOP,                         // 10
        0x1C30_1851,                 // 11 op words
        0x49BE_0BE0,                 // 12
        i(0x23, SP, A0, 0),          // 13 lw a0,0(sp): three words, no island
        r(8, RA, 0, 0, 0),           // 14 jr ra
        NOP,                         // 15
        0x49BE_0BE0,                 // 16 op word
        i(0x23, S0, V0, 352),        // 17 lw v0,352(s0): short, but it ends as the island at 4
        NOP,                         // 18
        r(9, RA, 0, S5, 0),          // 19 jalr s5,ra
        NOP,                         // 20
    ];
    let d = Discovery::run(&exe(&w));
    assert_eq!(d.entries.get(&at(4)), Some(&Origin::Island));
    assert!((4..=10).all(|n| d.code.contains(&at(n))));
    assert!(!d.code.contains(&at(11)) && !d.code.contains(&at(13)));
    assert_eq!(d.entries.get(&at(17)), Some(&Origin::Island));
    assert!((17..=20).all(|n| d.code.contains(&at(n))));
    // The same islands in a file's bytes, at their offsets.
    let bytes: Vec<u8> = w.iter().flat_map(|w| w.to_le_bytes()).collect();
    assert_eq!(psx2psp_recomp::discover::islands(&bytes), vec![(16, 44), (68, 84)]);
}

/// A module of 256 words whose code is linked at `link` (0: zero-based): `main` calls eleven
/// functions and switches on `a1` through a two-entry table at word 240, whose address it builds
/// with `lui`/`lw` and whose entries are its cases' addresses (words 32 and 34) from `link`.
fn module(link: u32) -> Vec<u8> {
    let mut w = vec![NOP; 256];
    let jal = |n: u32| (3 << 26) | (((link + 4 * n) >> 2) & 0x03FF_FFFF);
    let jmp = |n: u32| (2 << 26) | (((link + 4 * n) >> 2) & 0x03FF_FFFF);
    w[0] = i(9, SP, SP, (-8i16) as u16); //       main: addiu sp,sp,-8
    for k in 0..11 {
        w[1 + 2 * k] = jal(48 + 16 * k as u32); //  jal f_k; nop
    }
    let code = [
        i(0xB, A1, AT, 2),                        // 23 sltiu at,a1,2
        i(4, AT, ZERO, 11),                       // 24 beq at,zero,36
        r(0, 0, A1, T0, 2),                       // 25 sll t0,a1,2
        i(0xF, 0, T1, (link >> 16) as u16),       // 26 lui t1,hi(link)
        r(0x21, T1, T0, T1, 0),                   // 27 addu t1,t1,t0
        i(0x23, T1, T0, 4 * 240),                 // 28 lw t0,960(t1)
        NOP,                                      // 29
        r(8, T0, 0, 0, 0),                        // 30 jr t0
        NOP,                                      // 31
        jmp(36),                                  // 32 case 0: j 36
        NOP,                                      // 33
        jmp(36),                                  // 34 case 1: j 36
        NOP,                                      // 35
        r(8, RA, 0, 0, 0),                        // 36 jr ra
        i(9, SP, SP, 8),                          // 37 addiu sp,sp,8
    ];
    w[23..23 + code.len()].copy_from_slice(&code);
    for k in 0..11 {
        let f = 48 + 16 * k;
        w[f] = i(9, SP, SP, (-16i16) as u16); //    f_k: addiu sp,sp,-16
        w[f + 1] = r(8, RA, 0, 0, 0); //            jr ra
        w[f + 2] = i(9, SP, SP, 16); //             addiu sp,sp,16
    }
    w[240] = link + 4 * 32;
    w[241] = link + 4 * 34;
    w.iter().flat_map(|w| w.to_le_bytes()).collect()
}

/// A zero-based module is found by its calls naming its prologues as offsets from 0, at a 2 KB
/// sector of its file; code linked at its address is not one.
#[test]
fn zero_based_modules_are_found_at_their_sector() {
    use psx2psp_recomp::overlay;
    let (zero, linked) = (module(0), module(0x8001_0000));
    assert!(overlay::zero_based_start(&zero));
    assert!(!overlay::zero_based_start(&linked));
    // A file: the linked code four words in (its prologues are not where the module's calls
    // land), padding, the module at its second sector.
    let mut file = vec![0; 16];
    file.extend(&linked);
    file.resize(0x800, 0);
    file.extend(&zero);
    assert_eq!(overlay::zero_based(&file, 0), [0x800]);
    // The same bytes from 0x400 of the file: the module is at 0x400 of them.
    assert_eq!(overlay::zero_based(&file[0x400..], 0x400), [0x400]);
    assert_eq!(overlay::zero_based(&linked, 0), Vec::<u32>::new());
}

/// Discovered at 80000000, a zero-based module's switch table and cases are its offsets there;
/// in a text that is not a module the table at offset 960 is not read (only the jumps after the
/// `jr` are taken as its cases).
#[test]
fn zero_based_switch_tables_are_offsets_in_the_module() {
    let base = 0x8000_0000;
    let x = PsxExe { pc0: base, gp0: 0, text_addr: base, text: module(0), bss: (0, 0), stack: (0, 0) };
    let d = Discovery::run_file(&x, &[], &[], &[]);
    let o = |n: u32| base + 4 * n;
    assert_eq!(d.jumps[&o(30)], JumpReg::Table { table: o(240), targets: vec![o(32), o(34)], bounded: true });
    assert!(d.code.contains(&o(32)) && d.code.contains(&o(34)));
    // Four calls: under 8 votes, not a module.
    let mut few = module(0);
    few[4 * 9..4 * 23].fill(0);
    let x = PsxExe { text: few, ..x };
    assert_eq!(Discovery::run_file(&x, &[], &[], &[]).jumps[&o(30)], JumpReg::Table { table: o(32), targets: vec![o(32), o(34)], bounded: false });
}

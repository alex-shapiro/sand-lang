//! Register liveness: the registers that a frame may still read, and those
//! of its bindings in scope. The garbage collector clears the other registers
//! of each frame, so that temporaries and out-of-scope locals do not keep
//! values alive.

use super::bytecode::{NO_REG, Op, Piece, Program, Proto, Reg};
use super::value::Value;

/// How an instruction uses a run of registers.
#[derive(Clone, Copy, PartialEq)]
enum Access {
    Read,
    /// Overwritten on every path that continues in the frame.
    Write,
}

/// Calls `f` for each run of registers that `op`, in `proto`, reads or
/// overwrites. A conditional write is not a `Write`.
fn accesses(prog: &Program, proto: &Proto, op: &Op, mut f: impl FnMut(Access, Reg, usize)) {
    use Access::*;
    let nparams = |p: u32| prog.protos.get(p as usize).map_or(0, |p| p.nparams as usize);
    let template = |t: u32, f: &mut dyn FnMut(Access, Reg, usize)| {
        for piece in prog.templates.get(t as usize).map_or(&[][..], |t| &t.pieces) {
            if let Piece::Value(r) | Piece::Format(r, _) = piece {
                f(Read, *r, 1);
            }
        }
    };
    match *op {
        Op::Nop | Op::Jmp { .. } | Op::Loop { .. } | Op::RetUnit | Op::Unwind | Op::Internal { .. } => {}
        Op::LoadUnit { dst }
        | Op::LoadBool { dst, .. }
        | Op::LoadInt { dst, .. }
        | Op::LoadConst { dst, .. }
        | Op::LoadTag { dst, .. }
        | Op::LoadGlobal { dst, .. } => f(Write, dst, 1),
        Op::MakeClosure { dst, proto: p } => {
            for &r in prog.protos.get(p as usize).map_or(&[][..], |p| &p.captures) {
                if r != NO_REG {
                    f(Read, r, 1);
                }
            }
            f(Write, dst, 1);
        }
        // `LoadCell` writes `dst` if `cell` holds a cell, which the checker
        // ensures by rejecting locals used before they are declared.
        // `ListElem` follows a test of the list's length.
        Op::Move { dst, src }
        | Op::LoadCell { dst, cell: src }
        | Op::ListElem { dst, list: src, .. }
        | Op::NewCell { dst, src }
        | Op::Payload { dst, src }
        | Op::NewIter { dst, src }
        | Op::Len { dst, src }
        | Op::IntToFloat { dst, src }
        | Op::StrClass { dst, src, .. }
        | Op::ToStr { dst, src }
        | Op::Debug { dst, src }
        | Op::Spawn { dst, closure: src }
        | Op::ListPop { dst, list: src }
        | Op::ListRest { dst, list: src, .. }
        | Op::GetField { dst, obj: src, .. }
        | Op::AddIK { dst, a: src, .. }
        | Op::MulIK { dst, a: src, .. }
        | Op::DivIK { dst, a: src, .. }
        | Op::RemIK { dst, a: src, .. }
        | Op::NegI { dst, a: src }
        | Op::BitNot { dst, a: src }
        | Op::NegF { dst, a: src }
        | Op::Neg { dst, a: src }
        | Op::Not { dst, a: src }
        | Op::EqIK { dst, a: src, .. }
        | Op::NeIK { dst, a: src, .. }
        | Op::LtIK { dst, a: src, .. }
        | Op::LeIK { dst, a: src, .. }
        | Op::GtIK { dst, a: src, .. }
        | Op::GeIK { dst, a: src, .. } => {
            f(Read, src, 1);
            f(Write, dst, 1);
        }
        Op::AddI { dst, a, b }
        | Op::SubI { dst, a, b }
        | Op::MulI { dst, a, b }
        | Op::DivI { dst, a, b }
        | Op::RemI { dst, a, b }
        | Op::PowI { dst, a, b }
        | Op::BitAnd { dst, a, b }
        | Op::BitOr { dst, a, b }
        | Op::BitXor { dst, a, b }
        | Op::Shl { dst, a, b }
        | Op::Shr { dst, a, b }
        | Op::AddF { dst, a, b }
        | Op::SubF { dst, a, b }
        | Op::MulF { dst, a, b }
        | Op::DivF { dst, a, b }
        | Op::RemF { dst, a, b }
        | Op::PowF { dst, a, b }
        | Op::Add { dst, a, b }
        | Op::Sub { dst, a, b }
        | Op::Mul { dst, a, b }
        | Op::Div { dst, a, b }
        | Op::Rem { dst, a, b }
        | Op::Pow { dst, a, b }
        | Op::EqI { dst, a, b }
        | Op::NeI { dst, a, b }
        | Op::LtI { dst, a, b }
        | Op::LeI { dst, a, b }
        | Op::EqF { dst, a, b }
        | Op::NeF { dst, a, b }
        | Op::LtF { dst, a, b }
        | Op::LeF { dst, a, b }
        | Op::Eq { dst, a, b }
        | Op::Ne { dst, a, b }
        | Op::Lt { dst, a, b }
        | Op::Le { dst, a, b }
        | Op::NewRange { dst, a, b }
        | Op::NewRangeIncl { dst, a, b }
        | Op::Index { dst, obj: a, idx: b }
        | Op::IndexMap { dst, obj: a, key: b }
        | Op::ListGet { dst, list: a, idx: b }
        | Op::MapGet { dst, map: a, key: b }
        | Op::MapHas { dst, map: a, key: b }
        | Op::SetHas { dst, set: a, key: b }
        | Op::SetAdd { dst, set: a, key: b } => {
            f(Read, a, 1);
            f(Read, b, 1);
            f(Write, dst, 1);
        }
        // The previous value of `dst` is the default.
        Op::ListGetOr { dst, list: a, idx: b } | Op::MapGetOr { dst, map: a, key: b } => {
            f(Read, a, 1);
            f(Read, b, 1);
            f(Read, dst, 1);
        }
        Op::StoreGlobal { src, .. }
        | Op::JmpIf { cond: src, .. }
        | Op::JmpIfNot { cond: src, .. }
        | Op::JmpIfBack { cond: src, .. }
        | Op::JmpIfNotBack { cond: src, .. }
        | Op::JNeqK { a: src, .. }
        | Op::JNneK { a: src, .. }
        | Op::JNltIK { a: src, .. }
        | Op::JNleIK { a: src, .. }
        | Op::JNgtIK { a: src, .. }
        | Op::JNgeIK { a: src, .. }
        | Op::JNeqIK { a: src, .. }
        | Op::JNneIK { a: src, .. }
        | Op::JNltIKBack { a: src, .. }
        | Op::JNleIKBack { a: src, .. }
        | Op::JNgtIKBack { a: src, .. }
        | Op::JNgeIKBack { a: src, .. }
        | Op::JNeqIKBack { a: src, .. }
        | Op::JNneIKBack { a: src, .. }
        | Op::JNotSome { src, .. }
        | Op::JNotOk { src, .. }
        | Op::JSomeBack { src, .. }
        | Op::JNotVariant { src, .. }
        | Op::SwitchVariant { src, .. }
        | Op::SwitchInt { src, .. }
        | Op::SwitchChar { src, .. }
        | Op::JListLen { src, .. }
        | Op::JListMinLen { src, .. }
        | Op::JStrClass { src, .. }
        | Op::CheckRepeat { src }
        | Op::FreezeStr { reg: src }
        | Op::Ret { src }
        | Op::RetVariant { src, .. }
        | Op::TryTop { src }
        | Op::ForSet { base: src, .. }
        | Op::ForSetBack { base: src, .. }
        | Op::ForMap { base: src, .. }
        | Op::ForMapBack { base: src, .. } => f(Read, src, 1),
        Op::JNltI { a, b, .. }
        | Op::JNleI { a, b, .. }
        | Op::JNeqI { a, b, .. }
        | Op::JNneI { a, b, .. }
        | Op::JNltF { a, b, .. }
        | Op::JNleF { a, b, .. }
        | Op::JNeq { a, b, .. }
        | Op::JNne { a, b, .. }
        | Op::JNltIBack { a, b, .. }
        | Op::JNleIBack { a, b, .. }
        | Op::JNeqIBack { a, b, .. }
        | Op::JNneIBack { a, b, .. }
        | Op::StoreCell { cell: a, src: b }
        | Op::SetField { obj: a, src: b, .. }
        | Op::ListPush { list: a, src: b } => {
            f(Read, a, 1);
            f(Read, b, 1);
        }
        Op::SetIndex { obj, idx, src } | Op::SetIndexMap { obj, key: idx, src } => {
            f(Read, obj, 1);
            f(Read, idx, 1);
            f(Read, src, 1);
        }
        Op::ForRange { base, .. } | Op::ForRangeBack { base, .. } | Op::ForChars { base, .. } | Op::ForCharsBack { base, .. } => {
            f(Read, base, 2)
        }
        Op::ForList { base, .. } | Op::ForListBack { base, .. } | Op::ForRangeIncl { base, .. } | Op::ForRangeInclBack { base, .. } => {
            f(Read, base, 3)
        }
        Op::UnpackRange { base, src } => {
            f(Read, src, 1);
            f(Write, base, 3);
        }
        Op::CallR { callee, base, dst, argc } => {
            f(Read, callee, 1);
            f(Read, base, argc as usize);
            f(Write, dst, 1);
        }
        Op::CallDirect { base, proto: p } => {
            f(Read, base, nparams(p));
            f(Write, base, 1);
        }
        Op::CallDirectTo { base, dst, proto: p } => {
            f(Read, base, nparams(p as u32));
            f(Write, dst, 1);
        }
        Op::CallSelf { base, dst } => {
            f(Read, base, proto.nparams as usize);
            f(Write, dst, 1);
        }
        Op::CallNative { base, native } => {
            f(Read, base, prog.native_argc.get(native as usize).map_or(0, |&n| n as usize));
            f(Write, base, 1);
        }
        Op::CallNativeTo { base, dst, native } => {
            f(Read, base, prog.native_argc.get(native as usize).map_or(0, |&n| n as usize));
            f(Write, dst, 1);
        }
        // Only a native's tail call continues in the frame, with its result in `base`.
        Op::TailCall { callee, base, argc } => {
            f(Read, callee, 1);
            f(Read, base, argc as usize);
            f(Write, base, 1);
        }
        Op::TailCallDirect { base, proto: p } => f(Read, base, nparams(p)),
        Op::NewList { dst, start, count } | Op::NewTuple { dst, start, count } => {
            f(Read, start, count as usize);
            f(Write, dst, 1);
        }
        Op::NewMap { dst, start, count } => {
            f(Read, start, 2 * count as usize);
            f(Write, dst, 1);
        }
        Op::NewStruct { dst, start, def } => {
            f(Read, start, prog.types.get(def as usize).map_or(0, |t| t.fields.len()));
            f(Write, dst, 1);
        }
        Op::NewVariant { dst, start, variant } => {
            f(Read, start, prog.variants.get(variant as usize).map_or(1, |v| v.arity.max(1)));
            f(Write, dst, 1);
        }
        Op::Slice { dst, base, flags } => {
            f(Read, base, 1);
            if flags & 1 != 0 {
                f(Read, base + 1, 1);
            }
            if flags & 2 != 0 {
                f(Read, base + 2, 1);
            }
            f(Write, dst, 1);
        }
        Op::MapGetTuple { dst, map, start, n } | Op::MapHasTuple { dst, map, start, n } => {
            f(Read, map, 1);
            f(Read, start, n as usize);
            f(Write, dst, 1);
        }
        Op::AppendStr { local, src } => {
            f(Read, local, 1);
            f(Read, src, 1);
            f(Write, local, 1);
        }
        Op::Interp { dst, tmpl } => {
            template(tmpl, &mut f);
            f(Write, dst, 1);
        }
        Op::AppendInterp { local, tmpl } => {
            template(tmpl, &mut f);
            f(Read, local, 1);
            f(Write, local, 1);
        }
        Op::Describe { base, tmpl } => {
            f(Read, base, prog.desc_templates.get(tmpl as usize).map_or(0, |t| t.arity()));
            f(Write, base, 1);
        }
    }
}

/// Calls `f` for each run of registers that `op` overwrites when it
/// continues at its branch target (if `taken`) or at the next instruction.
fn edge_writes(op: &Op, taken: bool, mut f: impl FnMut(Reg, usize)) {
    match *op {
        Op::JNotSome { dst: r, .. }
        | Op::JNotOk { dst: r, .. }
        | Op::ForList { elem: r, .. }
        | Op::ForRange { var: r, .. }
        | Op::ForRangeIncl { var: r, .. }
        | Op::ForSet { elem: r, .. }
        | Op::ForChars { elem: r, .. }
            if !taken =>
        {
            f(r, 1)
        }
        Op::JSomeBack { dst: r, .. }
        | Op::ForListBack { elem: r, .. }
        | Op::ForRangeBack { var: r, .. }
        | Op::ForRangeInclBack { var: r, .. }
        | Op::ForSetBack { elem: r, .. }
        | Op::ForCharsBack { elem: r, .. }
            if taken =>
        {
            f(r, 1)
        }
        Op::ForMap { key, .. } if !taken => f(key, 2),
        Op::ForMapBack { key, .. } if taken => f(key, 2),
        _ => {}
    }
}

/// Calls `f` with each instruction that may run after the one at `at`
/// in the same frame, other than cleanup pads.
fn successors(prog: &Program, code: &[Op], at: usize, mut f: impl FnMut(usize)) {
    let op = &code[at];
    if let Some(off) = op.branch_off() {
        f((at as i64 + 1 + off as i64) as usize);
    }
    if let Op::SwitchVariant { table, .. } | Op::SwitchInt { table, .. } | Op::SwitchChar { table, .. } = *op
        && let Some(sw) = prog.switches.get(table as usize)
    {
        sw.targets.iter().for_each(|&t| f(t as usize));
        f(sw.default as usize);
    }
    if !op.is_unconditional() && at + 1 < code.len() {
        f(at + 1);
    }
}

/// Liveness tables are not built for protos whose table would take more
/// bytes than this. Their frames keep all registers.
const MAX_BYTES: usize = 8 << 20;

/// Blocks are split after this many instructions, which bounds the
/// instructions a collection scans for each frame.
const MAX_BLOCK: usize = 64;

/// The live registers of a proto on entry to each of its basic blocks.
pub struct Liveness {
    /// The first instruction of each block, ascending.
    starts: Vec<u32>,
    /// The registers that some block reads before it writes them, ascending.
    /// Only these can be live on entry to a block.
    globals: Vec<Reg>,
    /// `words` bits per block: the `globals` live on entry.
    live: Vec<u64>,
    words: usize,
}

impl Liveness {
    /// Solves liveness for `proto`, or returns the size of its table in
    /// bytes if that is more than `max_bytes`.
    pub fn new(prog: &Program, proto: &Proto, max_bytes: usize) -> Result<Liveness, usize> {
        let code = &proto.code;
        let n = code.len();
        let mut leader: Vec<bool> = (0..=n).map(|at| at % MAX_BLOCK == 0).collect();
        for at in 0..n {
            let mut ends = code[at].is_unconditional();
            successors(prog, code, at, |t| {
                if t != at + 1 {
                    leader[t.min(n)] = true;
                    ends = true;
                }
            });
            leader[at + 1] |= ends;
        }
        for c in &proto.cleanups {
            leader[(c.pad as usize).min(n)] = true;
        }
        let starts: Vec<u32> = (0..n).filter(|&i| leader[i]).map(|i| i as u32).collect();
        let nregs = proto.nregs as usize;
        let (mut global, mut written) = (vec![false; nregs], vec![false; nregs]);
        for (b, &start) in starts.iter().enumerate() {
            written.fill(false);
            for op in &code[start as usize..starts.get(b + 1).map_or(n, |&s| s as usize)] {
                accesses(prog, proto, op, |a, r, len| {
                    for r in (r as usize..(r as usize).saturating_add(len)).take_while(|&r| r < nregs) {
                        match a {
                            Access::Read => global[r] |= !written[r],
                            Access::Write => written[r] = true,
                        }
                    }
                });
            }
        }
        let globals: Vec<Reg> = (0..nregs).filter(|&r| global[r]).map(|r| r as Reg).collect();
        let words = globals.len().div_ceil(64);
        let size =
            starts.len() * size_of::<u32>() + globals.len() * size_of::<Reg>() + starts.len().saturating_mul(words) * size_of::<u64>();
        if size > max_bytes {
            return Err(size);
        }
        let nblocks = starts.len();
        let mut lv = Liveness { live: vec![0; nblocks * words], starts, globals, words };
        let mut preds = vec![Vec::new(); nblocks];
        for b in 0..nblocks {
            let (start, end) = lv.bounds(b, n);
            successors(prog, code, end - 1, |t| {
                if t < n {
                    preds[lv.block(t)].push(b as u32);
                }
            });
            for at in start..end {
                if let Some(pad) = proto.cleanup_at(at).filter(|&p| p < n) {
                    preds[lv.block(pad)].push(b as u32);
                }
            }
        }
        // Backward problem: visiting later blocks first converges fastest.
        let mut work: Vec<u32> = (0..nblocks as u32).collect();
        let mut queued = vec![true; nblocks];
        let (mut out, mut row) = (vec![0; nregs.div_ceil(64)], vec![0; words]);
        while let Some(b) = work.pop() {
            let b = b as usize;
            queued[b] = false;
            lv.scan(prog, proto, b, lv.starts[b] as usize, &mut out);
            for (i, &g) in lv.globals.iter().enumerate() {
                let bit = (out[g as usize / 64] >> (g % 64)) & 1;
                row[i / 64] = row[i / 64] & !(1 << (i % 64)) | bit << (i % 64);
            }
            let stored = &mut lv.live[b * words..(b + 1) * words];
            if *stored != *row {
                stored.copy_from_slice(&row);
                for &p in &preds[b] {
                    if !std::mem::replace(&mut queued[p as usize], true) {
                        work.push(p);
                    }
                }
            }
        }
        Ok(lv)
    }

    /// Heap memory of the table, in bytes.
    pub fn bytes(&self) -> usize {
        self.starts.capacity() * size_of::<u32>() + self.globals.capacity() * size_of::<Reg>() + self.live.capacity() * size_of::<u64>()
    }

    /// Writes to `out` (one bit per register of `proto`) the registers live
    /// on entry to instruction `pos`: those that the code may read and the
    /// bindings in scope (`Proto::scopes`).
    pub fn live_at(&self, prog: &Program, proto: &Proto, pos: usize, out: &mut [u64]) {
        self.scan(prog, proto, self.block(pos), pos, out);
        for sc in proto.scopes.iter().filter(|sc| (sc.start as usize..sc.end as usize).contains(&pos)) {
            if let Some(w) = out.get_mut(sc.reg as usize / 64) {
                *w |= 1 << (sc.reg % 64);
            }
        }
    }

    /// The block of instruction `at`.
    fn block(&self, at: usize) -> usize {
        self.starts.partition_point(|&s| s as usize <= at) - 1
    }

    /// The first instruction of block `b` and one past its last.
    fn bounds(&self, b: usize, n: usize) -> (usize, usize) {
        (self.starts[b] as usize, self.starts.get(b + 1).map_or(n, |&s| s as usize))
    }

    /// Adds to `out` the registers live on entry to the block of instruction `at`.
    fn add_entry(&self, at: usize, out: &mut [u64]) {
        let b = self.block(at);
        for (w, &bits) in self.live[b * self.words..(b + 1) * self.words].iter().enumerate() {
            let mut bits = bits;
            while bits != 0 {
                let g = self.globals[w * 64 + bits.trailing_zeros() as usize] as usize;
                out[g / 64] |= 1 << (g % 64);
                bits &= bits - 1;
            }
        }
    }

    /// Writes to `out` the registers that the code may read after entering
    /// instruction `pos` of block `b`, from the stored entries of the blocks
    /// that follow.
    fn scan(&self, prog: &Program, proto: &Proto, b: usize, pos: usize, out: &mut [u64]) {
        let code = &proto.code;
        let (_, end) = self.bounds(b, code.len());
        out.fill(0);
        let nregs = proto.nregs as usize;
        let set = |out: &mut [u64], r: Reg, len: usize, on: bool| {
            for r in (r as usize..(r as usize).saturating_add(len)).take_while(|&r| r < nregs) {
                if on {
                    out[r / 64] |= 1 << (r % 64);
                } else {
                    out[r / 64] &= !(1 << (r % 64));
                }
            }
        };
        let last = end - 1;
        let target = code[last].branch_off().map(|off| (last as i64 + 1 + off as i64) as usize);
        let mut edge = vec![0; out.len()];
        successors(prog, code, last, |t| {
            if t >= code.len() {
                return;
            }
            edge.fill(0);
            self.add_entry(t, &mut edge);
            // A branch to the next instruction writes on neither edge.
            if target != Some(last + 1) {
                edge_writes(&code[last], Some(t) == target, |r, len| set(&mut edge, r, len, false));
            }
            or(out, &edge);
        });
        for at in (pos..end).rev() {
            accesses(prog, proto, &code[at], |a, r, len| {
                if a == Access::Write {
                    set(out, r, len, false);
                }
            });
            accesses(prog, proto, &code[at], |a, r, len| {
                if a == Access::Read {
                    set(out, r, len, true);
                }
            });
            // Cancelling the task runs the pad from any instruction in its
            // range, before that instruction writes anything.
            if let Some(pad) = proto.cleanup_at(at)
                && pad < code.len()
            {
                self.add_entry(pad, out);
            }
        }
    }
}

fn or(out: &mut [u64], row: &[u64]) {
    for (o, r) in out.iter_mut().zip(row) {
        *o |= r;
    }
}

/// The liveness tables of the protos whose frames collections have seen.
#[derive(Default)]
pub struct LivenessCache {
    /// By proto: `None` if not solved yet, `Some(None)` if too large.
    tables: Vec<Option<Option<Liveness>>>,
    bytes: usize,
    /// The live registers of the last frame cleared: of proto `last.0`
    /// stopped at `last.1`. Recursive calls stop many frames at one place.
    row: Vec<u64>,
    last: Option<(u32, usize)>,
}

impl LivenessCache {
    /// Heap memory of the tables, in bytes.
    pub fn bytes(&self) -> usize {
        self.bytes + self.tables.capacity() * size_of::<Option<Option<Liveness>>>() + self.row.capacity() * size_of::<u64>()
    }

    /// Clears the registers of `regs`, the start of the register window of
    /// a frame of proto `p` stopped at instruction `pos`, to `dead` if the
    /// frame never reads them again. Solves the proto's liveness first if
    /// needed, unless its table would take more than `room` bytes, which
    /// the table's size is then taken from.
    #[inline]
    pub fn clear_dead(&mut self, prog: &Program, p: u32, pos: usize, regs: &mut [Value], dead: Value, room: &mut usize) {
        let Some(proto) = prog.protos.get(p as usize) else { return };
        if pos >= proto.code.len() {
            return;
        }
        let i = p as usize;
        if self.tables.len() <= i {
            self.tables.resize_with(i + 1, || None);
        }
        if self.tables[i].is_none() {
            match Liveness::new(prog, proto, MAX_BYTES.min(*room)) {
                Ok(lv) => {
                    *room = room.saturating_sub(lv.bytes());
                    self.bytes += lv.bytes();
                    self.tables[i] = Some(Some(lv));
                }
                Err(size) if size > MAX_BYTES => self.tables[i] = Some(None),
                Err(_) => return,
            }
        }
        let Some(Some(lv)) = &self.tables[i] else { return };
        if self.last != Some((p, pos)) {
            self.row.resize((proto.nregs as usize).div_ceil(64), 0);
            lv.live_at(prog, proto, pos, &mut self.row);
            self.last = Some((p, pos));
        }
        for (chunk, &live) in regs.chunks_mut(64).zip(&self.row) {
            let mut bits = !live;
            while bits != 0 && (bits.trailing_zeros() as usize) < chunk.len() {
                chunk[bits.trailing_zeros() as usize] = dead;
                bits &= bits - 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, Engine};
    use std::path::Path;
    use std::sync::Arc;

    fn compile(src: &str) -> Option<Arc<Program>> {
        let engine = Engine::new(Config::new()).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let root = std::env::current_dir().unwrap();
        rt.block_on(engine.compile("main.sand", src, &root, None)).ok().map(|c| c.program)
    }

    /// The programs of the spec cases and examples that compile.
    fn corpus() -> Vec<Arc<Program>> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut sources = Vec::new();
        for dir in ["tests/spec", "examples"] {
            for e in std::fs::read_dir(root.join(dir)).unwrap() {
                let path = e.unwrap().path();
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                match path.extension().and_then(|x| x.to_str()) {
                    Some("test") => {
                        for case in text.split("\n=== ").skip(1) {
                            let body = case.split_once('\n').map_or("", |c| c.1);
                            sources.push(body.split("\n--- ").next().unwrap().to_string());
                        }
                    }
                    Some("sand") => sources.push(text),
                    _ => {}
                }
            }
        }
        let programs: Vec<_> = sources.iter().filter_map(|s| compile(s)).collect();
        assert!(programs.len() > 500, "only {} programs compiled", programs.len());
        programs
    }

    fn has(row: &[u64], r: usize) -> bool {
        row[r / 64] & (1 << (r % 64)) != 0
    }

    /// Liveness solved per instruction until nothing changes.
    fn reference(prog: &Program, proto: &Proto) -> Vec<Vec<u64>> {
        let (code, words, nregs) = (&proto.code, (proto.nregs as usize).div_ceil(64), proto.nregs as usize);
        let set = |row: &mut [u64], r: Reg, len: usize, on: bool| {
            for r in (r as usize..r as usize + len).filter(|&r| r < nregs) {
                if on { row[r / 64] |= 1 << (r % 64) } else { row[r / 64] &= !(1 << (r % 64)) }
            }
        };
        let mut live = vec![vec![0u64; words]; code.len()];
        loop {
            let mut changed = false;
            for at in (0..code.len()).rev() {
                let mut row = vec![0; words];
                let target = code[at].branch_off().map(|off| (at as i64 + 1 + off as i64) as usize);
                successors(prog, code, at, |t| {
                    let Some(edge) = live.get(t) else { return };
                    let mut edge = edge.clone();
                    if target != Some(at + 1) {
                        edge_writes(&code[at], Some(t) == target, |r, len| set(&mut edge, r, len, false));
                    }
                    or(&mut row, &edge);
                });
                accesses(prog, proto, &code[at], |a, r, len| {
                    if a == Access::Write {
                        set(&mut row, r, len, false)
                    }
                });
                accesses(prog, proto, &code[at], |a, r, len| {
                    if a == Access::Read {
                        set(&mut row, r, len, true)
                    }
                });
                if let Some(pad) = proto.cleanup_at(at) {
                    let pad_row = live[pad].clone();
                    or(&mut row, &pad_row);
                }
                if row != live[at] {
                    live[at] = row;
                    changed = true;
                }
            }
            if !changed {
                return live;
            }
        }
    }

    #[test]
    fn liveness_matches_a_per_instruction_solution() {
        for prog in corpus() {
            for proto in &prog.protos {
                let lv = Liveness::new(&prog, proto, MAX_BYTES).unwrap();
                let mut row = vec![0; (proto.nregs as usize).div_ceil(64)];
                for (pos, mut want) in reference(&prog, proto).into_iter().enumerate() {
                    for sc in proto.scopes.iter().filter(|sc| (sc.start as usize..sc.end as usize).contains(&pos)) {
                        want[sc.reg as usize / 64] |= 1 << (sc.reg % 64);
                    }
                    lv.live_at(&prog, proto, pos, &mut row);
                    assert_eq!(row, want, "{} at {pos}: {:?}", proto.name, proto.code[pos]);
                }
            }
        }
    }

    /// Frames read no register before they write it, other than their
    /// parameters and captures. A frame stopped in a call reads no register
    /// of the callee's window after the call, other than the result.
    #[test]
    fn frames_read_only_their_own_initialized_registers() {
        for prog in corpus() {
            for proto in &prog.protos {
                let lv = Liveness::new(&prog, proto, MAX_BYTES).unwrap();
                let mut row = vec![0; (proto.nregs as usize).div_ceil(64)];
                if proto.code.is_empty() {
                    continue;
                }
                lv.live_at(&prog, proto, 0, &mut row);
                let inputs = proto.nparams as usize + proto.captures.len();
                for r in inputs..proto.nregs as usize {
                    assert!(!has(&row, r), "{} reads register {r} before writing it: {:#?} {:?}", proto.name, proto.code, proto.scopes);
                }
                for (at, op) in proto.code.iter().enumerate() {
                    let (base, dst) = match *op {
                        Op::CallR { base, dst, .. } | Op::CallDirectTo { base, dst, .. } | Op::CallSelf { base, dst } => (base, dst),
                        Op::CallDirect { base, .. } => (base, base),
                        _ => continue,
                    };
                    if at + 1 == proto.code.len() {
                        continue;
                    }
                    lv.live_at(&prog, proto, at + 1, &mut row);
                    for r in (base as usize..proto.nregs as usize).filter(|&r| r != dst as usize) {
                        assert!(!has(&row, r), "{} reads register {r} of the callee at {at} after {op:?}", proto.name);
                    }
                }
            }
        }
    }

    /// A long script whose locals stay in scope to its end, with branches
    /// between them, needs a table about as large as its code.
    #[test]
    fn tables_grow_with_code_not_with_code_times_registers() {
        let mut src = String::from("let c = true\n");
        for i in 0..2000 {
            src += &format!("let x{i} = if c {{ \"a{i}\" }} else {{ \"b{i}\" }}\n");
        }
        src += "print(x0.len() + x1999.len())\n";
        let prog = compile(&src).unwrap();
        let proto = prog.protos.iter().max_by_key(|p| p.code.len()).unwrap();
        assert!(proto.nregs > 2000);
        let lv = Liveness::new(&prog, proto, MAX_BYTES).unwrap();
        assert!(lv.bytes() < 2 * proto.code.len() * size_of::<Op>(), "{} bytes for {} instructions", lv.bytes(), proto.code.len());
    }

    /// Without room for its table, a frame keeps all its registers; with
    /// room, the dead ones are cleared and the table's size is taken from it.
    #[test]
    fn frames_keep_their_registers_without_room_for_a_table() {
        let prog = compile("fn f(a: str) -> int {\n    let b = a.repeat(2)\n    b.len()\n}\nprint(f(\"x\"))").unwrap();
        let p = prog.protos.iter().position(|p| p.name == "f").unwrap();
        let proto = &prog.protos[p];
        let pos = proto.code.len() - 1;
        let regs = || vec![Value::Int(1); proto.nregs as usize];
        let mut cache = LivenessCache::default();
        let (mut kept, mut room) = (regs(), 0);
        cache.clear_dead(&prog, p as u32, pos, &mut kept, Value::Unit, &mut room);
        assert_eq!(kept, regs());
        let (mut cleared, mut room) = (regs(), usize::MAX);
        cache.clear_dead(&prog, p as u32, pos, &mut cleared, Value::Unit, &mut room);
        assert!(cleared.contains(&Value::Unit) && room < usize::MAX, "{cleared:?}");
    }
}

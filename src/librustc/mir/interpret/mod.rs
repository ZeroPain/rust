// Copyright 2018 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! An interpreter for MIR used in CTFE and by miri

#[macro_export]
macro_rules! err {
    ($($tt:tt)*) => { Err($crate::mir::interpret::EvalErrorKind::$($tt)*.into()) };
}

mod error;
mod value;
mod allocation;
mod pointer;

pub use self::error::{
    EvalError, EvalResult, EvalErrorKind, AssertMessage, ConstEvalErr, struct_error,
    FrameInfo, ConstEvalRawResult, ConstEvalResult, ErrorHandled,
};

pub use self::value::{Scalar, ScalarMaybeUndef, RawConst, ConstValue};

pub use self::allocation::{
    InboundsCheck, Allocation, AllocationExtra,
    Relocations, UndefMask,
};

pub use self::pointer::{Pointer, PointerArithmetic};

use std::fmt;
use mir;
use hir::def_id::DefId;
use ty::{self, TyCtxt, Instance};
use ty::layout::{self, Size};
use middle::region;
use std::io;
use std::hash::Hash;
use rustc_serialize::{Encoder, Decodable, Encodable};
use rustc_data_structures::fx::FxHashMap;
use rustc_data_structures::sync::{Lock as Mutex, HashMapExt};
use rustc_data_structures::tiny_list::TinyList;
use byteorder::{WriteBytesExt, ReadBytesExt, LittleEndian, BigEndian};
use ty::codec::TyDecoder;
use std::sync::atomic::{AtomicU32, Ordering};
use std::num::NonZeroU32;

#[derive(Clone, Debug, PartialEq, Eq, Hash, RustcEncodable, RustcDecodable)]
pub enum Lock {
    NoLock,
    WriteLock(DynamicLifetime),
    /// This should never be empty -- that would be a read lock held and nobody
    /// there to release it...
    ReadLock(Vec<DynamicLifetime>),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, RustcEncodable, RustcDecodable)]
pub struct DynamicLifetime {
    pub frame: usize,
    pub region: Option<region::Scope>, // "None" indicates "until the function ends"
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, RustcEncodable, RustcDecodable)]
pub enum AccessKind {
    Read,
    Write,
}

/// Uniquely identifies a specific constant or static.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, RustcEncodable, RustcDecodable)]
pub struct GlobalId<'tcx> {
    /// For a constant or static, the `Instance` of the item itself.
    /// For a promoted global, the `Instance` of the function they belong to.
    pub instance: ty::Instance<'tcx>,

    /// The index for promoted globals within their function's `Mir`.
    pub promoted: Option<mir::Promoted>,
}

#[derive(Copy, Clone, Eq, Hash, Ord, PartialEq, PartialOrd, Debug)]
pub struct AllocId(pub u64);

impl ::rustc_serialize::UseSpecializedEncodable for AllocId {}
impl ::rustc_serialize::UseSpecializedDecodable for AllocId {}

#[derive(RustcDecodable, RustcEncodable)]
enum AllocKind {
    Alloc,
    Fn,
    Static,
}

pub fn specialized_encode_alloc_id<
    'a, 'tcx,
    E: Encoder,
>(
    encoder: &mut E,
    tcx: TyCtxt<'a, 'tcx, 'tcx>,
    alloc_id: AllocId,
) -> Result<(), E::Error> {
    let alloc_type: AllocType<'tcx, &'tcx Allocation> =
        tcx.alloc_map.lock().get(alloc_id).expect("no value for AllocId");
    match alloc_type {
        AllocType::Memory(alloc) => {
            trace!("encoding {:?} with {:#?}", alloc_id, alloc);
            AllocKind::Alloc.encode(encoder)?;
            alloc.encode(encoder)?;
        }
        AllocType::Function(fn_instance) => {
            trace!("encoding {:?} with {:#?}", alloc_id, fn_instance);
            AllocKind::Fn.encode(encoder)?;
            fn_instance.encode(encoder)?;
        }
        AllocType::Static(did) => {
            // referring to statics doesn't need to know about their allocations,
            // just about its DefId
            AllocKind::Static.encode(encoder)?;
            did.encode(encoder)?;
        }
    }
    Ok(())
}

// Used to avoid infinite recursion when decoding cyclic allocations.
type DecodingSessionId = NonZeroU32;

#[derive(Clone)]
enum State {
    Empty,
    InProgressNonAlloc(TinyList<DecodingSessionId>),
    InProgress(TinyList<DecodingSessionId>, AllocId),
    Done(AllocId),
}

pub struct AllocDecodingState {
    // For each AllocId we keep track of which decoding state it's currently in.
    decoding_state: Vec<Mutex<State>>,
    // The offsets of each allocation in the data stream.
    data_offsets: Vec<u32>,
}

impl AllocDecodingState {

    pub fn new_decoding_session(&self) -> AllocDecodingSession<'_> {
        static DECODER_SESSION_ID: AtomicU32 = AtomicU32::new(0);
        let counter = DECODER_SESSION_ID.fetch_add(1, Ordering::SeqCst);

        // Make sure this is never zero
        let session_id = DecodingSessionId::new((counter & 0x7FFFFFFF) + 1).unwrap();

        AllocDecodingSession {
            state: self,
            session_id,
        }
    }

    pub fn new(data_offsets: Vec<u32>) -> AllocDecodingState {
        let decoding_state = vec![Mutex::new(State::Empty); data_offsets.len()];

        AllocDecodingState {
            decoding_state,
            data_offsets,
        }
    }
}

#[derive(Copy, Clone)]
pub struct AllocDecodingSession<'s> {
    state: &'s AllocDecodingState,
    session_id: DecodingSessionId,
}

impl<'s> AllocDecodingSession<'s> {

    // Decodes an AllocId in a thread-safe way.
    pub fn decode_alloc_id<'a, 'tcx, D>(&self,
                                        decoder: &mut D)
                                        -> Result<AllocId, D::Error>
        where D: TyDecoder<'a, 'tcx>,
              'tcx: 'a,
    {
        // Read the index of the allocation
        let idx = decoder.read_u32()? as usize;
        let pos = self.state.data_offsets[idx] as usize;

        // Decode the AllocKind now so that we know if we have to reserve an
        // AllocId.
        let (alloc_kind, pos) = decoder.with_position(pos, |decoder| {
            let alloc_kind = AllocKind::decode(decoder)?;
            Ok((alloc_kind, decoder.position()))
        })?;

        // Check the decoding state, see if it's already decoded or if we should
        // decode it here.
        let alloc_id = {
            let mut entry = self.state.decoding_state[idx].lock();

            match *entry {
                State::Done(alloc_id) => {
                    return Ok(alloc_id);
                }
                ref mut entry @ State::Empty => {
                    // We are allowed to decode
                    match alloc_kind {
                        AllocKind::Alloc => {
                            // If this is an allocation, we need to reserve an
                            // AllocId so we can decode cyclic graphs.
                            let alloc_id = decoder.tcx().alloc_map.lock().reserve();
                            *entry = State::InProgress(
                                TinyList::new_single(self.session_id),
                                alloc_id);
                            Some(alloc_id)
                        },
                        AllocKind::Fn | AllocKind::Static => {
                            // Fns and statics cannot be cyclic and their AllocId
                            // is determined later by interning
                            *entry = State::InProgressNonAlloc(
                                TinyList::new_single(self.session_id));
                            None
                        }
                    }
                }
                State::InProgressNonAlloc(ref mut sessions) => {
                    if sessions.contains(&self.session_id) {
                        bug!("This should be unreachable")
                    } else {
                        // Start decoding concurrently
                        sessions.insert(self.session_id);
                        None
                    }
                }
                State::InProgress(ref mut sessions, alloc_id) => {
                    if sessions.contains(&self.session_id) {
                        // Don't recurse.
                        return Ok(alloc_id)
                    } else {
                        // Start decoding concurrently
                        sessions.insert(self.session_id);
                        Some(alloc_id)
                    }
                }
            }
        };

        // Now decode the actual data
        let alloc_id = decoder.with_position(pos, |decoder| {
            match alloc_kind {
                AllocKind::Alloc => {
                    let allocation = <&'tcx Allocation as Decodable>::decode(decoder)?;
                    // We already have a reserved AllocId.
                    let alloc_id = alloc_id.unwrap();
                    trace!("decoded alloc {:?} {:#?}", alloc_id, allocation);
                    decoder.tcx().alloc_map.lock().set_id_same_memory(alloc_id, allocation);
                    Ok(alloc_id)
                },
                AllocKind::Fn => {
                    assert!(alloc_id.is_none());
                    trace!("creating fn alloc id");
                    let instance = ty::Instance::decode(decoder)?;
                    trace!("decoded fn alloc instance: {:?}", instance);
                    let alloc_id = decoder.tcx().alloc_map.lock().create_fn_alloc(instance);
                    Ok(alloc_id)
                },
                AllocKind::Static => {
                    assert!(alloc_id.is_none());
                    trace!("creating extern static alloc id at");
                    let did = DefId::decode(decoder)?;
                    let alloc_id = decoder.tcx().alloc_map.lock().intern_static(did);
                    Ok(alloc_id)
                }
            }
        })?;

        self.state.decoding_state[idx].with_lock(|entry| {
            *entry = State::Done(alloc_id);
        });

        Ok(alloc_id)
    }
}

impl fmt::Display for AllocId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, RustcDecodable, RustcEncodable)]
pub enum AllocType<'tcx, M> {
    /// The alloc id is used as a function pointer
    Function(Instance<'tcx>),
    /// The alloc id points to a "lazy" static variable that did not get computed (yet).
    /// This is also used to break the cycle in recursive statics.
    Static(DefId),
    /// The alloc id points to memory
    Memory(M)
}

pub struct AllocMap<'tcx, M> {
    /// Lets you know what an AllocId refers to
    id_to_type: FxHashMap<AllocId, AllocType<'tcx, M>>,

    /// Used to ensure that functions and statics only get one associated AllocId
    type_interner: FxHashMap<AllocType<'tcx, M>, AllocId>,

    /// The AllocId to assign to the next requested id.
    /// Always incremented, never gets smaller.
    next_id: AllocId,
}

impl<'tcx, M: fmt::Debug + Eq + Hash + Clone> AllocMap<'tcx, M> {
    pub fn new() -> Self {
        AllocMap {
            id_to_type: Default::default(),
            type_interner: Default::default(),
            next_id: AllocId(0),
        }
    }

    /// obtains a new allocation ID that can be referenced but does not
    /// yet have an allocation backing it.
    pub fn reserve(
        &mut self,
    ) -> AllocId {
        let next = self.next_id;
        self.next_id.0 = self.next_id.0
            .checked_add(1)
            .expect("You overflowed a u64 by incrementing by 1... \
                     You've just earned yourself a free drink if we ever meet. \
                     Seriously, how did you do that?!");
        next
    }

    fn intern(&mut self, alloc_type: AllocType<'tcx, M>) -> AllocId {
        if let Some(&alloc_id) = self.type_interner.get(&alloc_type) {
            return alloc_id;
        }
        let id = self.reserve();
        debug!("creating alloc_type {:?} with id {}", alloc_type, id);
        self.id_to_type.insert(id, alloc_type.clone());
        self.type_interner.insert(alloc_type, id);
        id
    }

    // FIXME: Check if functions have identity. If not, we should not intern these,
    // but instead create a new id per use.
    // Alternatively we could just make comparing function pointers an error.
    pub fn create_fn_alloc(&mut self, instance: Instance<'tcx>) -> AllocId {
        self.intern(AllocType::Function(instance))
    }

    pub fn get(&self, id: AllocId) -> Option<AllocType<'tcx, M>> {
        self.id_to_type.get(&id).cloned()
    }

    pub fn unwrap_memory(&self, id: AllocId) -> M {
        match self.get(id) {
            Some(AllocType::Memory(mem)) => mem,
            _ => bug!("expected allocation id {} to point to memory", id),
        }
    }

    pub fn intern_static(&mut self, static_id: DefId) -> AllocId {
        self.intern(AllocType::Static(static_id))
    }

    pub fn allocate(&mut self, mem: M) -> AllocId {
        let id = self.reserve();
        self.set_id_memory(id, mem);
        id
    }

    pub fn set_id_memory(&mut self, id: AllocId, mem: M) {
        if let Some(old) = self.id_to_type.insert(id, AllocType::Memory(mem)) {
            bug!("tried to set allocation id {}, but it was already existing as {:#?}", id, old);
        }
    }

    pub fn set_id_same_memory(&mut self, id: AllocId, mem: M) {
       self.id_to_type.insert_same(id, AllocType::Memory(mem));
    }
}

////////////////////////////////////////////////////////////////////////////////
// Methods to access integers in the target endianness
////////////////////////////////////////////////////////////////////////////////

pub fn write_target_uint(
    endianness: layout::Endian,
    mut target: &mut [u8],
    data: u128,
) -> Result<(), io::Error> {
    let len = target.len();
    match endianness {
        layout::Endian::Little => target.write_uint128::<LittleEndian>(data, len),
        layout::Endian::Big => target.write_uint128::<BigEndian>(data, len),
    }
}

pub fn read_target_uint(endianness: layout::Endian, mut source: &[u8]) -> Result<u128, io::Error> {
    match endianness {
        layout::Endian::Little => source.read_uint128::<LittleEndian>(source.len()),
        layout::Endian::Big => source.read_uint128::<BigEndian>(source.len()),
    }
}

////////////////////////////////////////////////////////////////////////////////
// Methods to facilitate working with signed integers stored in a u128
////////////////////////////////////////////////////////////////////////////////

pub fn sign_extend(value: u128, size: Size) -> u128 {
    let size = size.bits();
    // sign extend
    let shift = 128 - size;
    // shift the unsigned value to the left
    // and back to the right as signed (essentially fills with FF on the left)
    (((value << shift) as i128) >> shift) as u128
}

pub fn truncate(value: u128, size: Size) -> u128 {
    let size = size.bits();
    let shift = 128 - size;
    // truncate (shift left to drop out leftover values, shift right to fill with zeroes)
    (value << shift) >> shift
}

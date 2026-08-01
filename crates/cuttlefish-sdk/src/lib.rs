//! Guest-side library for writing cuttlefish proc-blocks.
//!
//! A block is a state machine that the host drives. It never calls the host and
//! waits; it *returns* a [`Command`] saying what it wants, and the host — having
//! done that thing — steps it again with an [`Event`]. See [`cuttlefish_abi`]
//! for why control is inverted, and what that buys: chiefly that cancellation
//! needs no cooperation from the guest, because the host simply stops stepping.
//!
//! This crate exists so block authors do not hand-write that inversion.
//! Implement [`Block`], call [`export_block!`], and the macro emits the raw wasm
//! exports the host expects.
//!
//! # Writing a block
//!
//! ```
//! use cuttlefish_sdk::{Block, Command, Event};
//!
//! #[derive(Default)]
//! struct Shout;
//!
//! impl Block for Shout {
//!     fn start(&mut self, input: serde_json::Value) -> Command {
//!         match input.get("path").and_then(|v| v.as_str()) {
//!             Some(path) => Command::Open { path: path.to_string() },
//!             None => Command::Fail {
//!                 code: "schema_validation_failed".into(),
//!                 message: "input needs a string `path`".into(),
//!             },
//!         }
//!     }
//!
//!     fn step(&mut self, event: Event) -> Command {
//!         match event {
//!             Event::Opened { handle, len, .. } => Command::Slice { handle, offset: 0, len },
//!             Event::Sliced { text, .. } => Command::Done {
//!                 result: serde_json::json!({ "shouted": text.to_uppercase() }),
//!             },
//!             other => Command::Fail {
//!                 code: "unexpected_event".into(),
//!                 message: format!("{other:?}"),
//!             },
//!         }
//!     }
//! }
//!
//! // A real block adds this to emit the wasm exports:
//! //     cuttlefish_sdk::export_block!(Shout);
//! ```
//!
//! Because a block is an ordinary Rust type, it can be unit-tested natively with
//! no wasm involved: construct it, call `start`, then feed it the events its
//! commands would produce. Only the boundary itself needs a wasm harness.
//!
//! # Memory ownership across the boundary
//!
//! Values handed to the host are leaked on purpose. The host reads them
//! immediately after the call returns, and the entire instance is destroyed when
//! the job ends, so there is nothing to reclaim and no cross-language allocator
//! coordination to get wrong.
//!
//! Do not "fix" this by freeing. The host would then read freed memory, and on
//! wasm that is a silent wrong answer rather than a segfault — the linear memory
//! is still perfectly valid to read, it just no longer holds what anyone thinks.

#![forbid(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

pub use cuttlefish_abi::{Command, Event, Handle, MediaKind, Signature, TokenAction, Ty};

/// How a guest hands a (pointer, length) pair back to the host.
///
/// The obvious alternative is packing both into the single `i64` a wasm export
/// can return. That works only while pointers are 32 bits. Returning a pointer
/// to this struct instead costs one extra memory read per call and keeps every
/// export signature unchanged under a 64-bit guest, where `usize` simply widens.
///
/// Do not replace this with bit-packing; that is precisely what it exists to
/// avoid. The host reads exactly `2 * size_of::<usize>()` bytes at the returned
/// address and splits them down the middle, so this layout is load-bearing —
/// hence `#[repr(C)]`, and hence the tests pinning its size and field order.
#[repr(C)]
pub struct Desc {
    /// Address of the payload.
    pub ptr: usize,
    /// Payload length, in bytes.
    pub len: usize,
}

/// What a proc-block author implements.
///
/// The host calls [`start`](Block::start) once with the job's input, then
/// [`step`](Block::step) after each command it carries out, until the block
/// returns [`Command::Done`] or [`Command::Fail`].
pub trait Block: Default {
    /// What this block accepts and produces.
    ///
    /// Declared here rather than in a file beside the block, so it cannot
    /// disagree with the code below it. The host reads this to typecheck a
    /// pipeline's seams before running anything.
    ///
    /// The default is permissive — JSON in, JSON out — so an existing block
    /// keeps working. That is deliberately the weakest useful answer: a pipeline
    /// of `Json` seams typechecks unconditionally, so a block that means to be
    /// composed should say something more specific.
    fn signature() -> Signature {
        Signature {
            input: Ty::Json,
            output: Ty::Json,
        }
    }

    /// Produce the first command from the job's input.
    ///
    /// Prefer returning [`Command::Fail`] to panicking on malformed input: a
    /// panic becomes an opaque wasm trap, whereas a `Fail` carries a code and a
    /// message the caller can act on.
    fn start(&mut self, input: serde_json::Value) -> Command;

    /// Produce the next command, given the result of the previous one.
    fn step(&mut self, event: Event) -> Command;

    /// Decide whether generation should continue, once per streamed token.
    ///
    /// Defaults to [`TokenAction::Continue`], so a block indifferent to
    /// streaming can ignore it. A token or two may still arrive after returning
    /// [`TokenAction::Stop`], because the verdict has to travel back to the
    /// thread doing the generating.
    fn on_token(&mut self, _token: &str) -> TokenAction {
        TokenAction::Continue
    }
}

/// Allocate a buffer for the host to write into, returning its address.
///
/// Leaked deliberately; see the crate docs on memory ownership.
#[doc(hidden)]
pub fn __alloc(len: usize) -> usize {
    let mut buf = Vec::<u8>::with_capacity(len);
    let ptr = buf.as_mut_ptr() as usize;
    std::mem::forget(buf);
    ptr
}

/// Decode JSON the host wrote at `ptr`.
///
/// # Safety
///
/// `ptr` must point at `len` initialized bytes written by the host, normally
/// into a buffer obtained from [`__alloc`].
#[doc(hidden)]
pub unsafe fn __read_json<T: serde::de::DeserializeOwned>(ptr: usize, len: usize) -> T {
    let slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };
    serde_json::from_slice(slice).expect("host sent malformed JSON")
}

/// Serialize `value` into guest memory, returning the address of a [`Desc`]
/// describing it.
///
/// Both the payload and the descriptor are leaked; see the crate docs.
#[doc(hidden)]
pub fn __write_json<T: serde::Serialize>(value: &T) -> usize {
    let bytes = serde_json::to_vec(value).expect("guest produced unserializable value");
    let len = bytes.len();
    let ptr = Box::into_raw(bytes.into_boxed_slice()) as *mut u8 as usize;
    Box::into_raw(Box::new(Desc { ptr, len })) as usize
}

/// Emit the wasm exports for a [`Block`] implementation.
///
/// The block's state lives in a thread-local because the host instantiates one
/// module per job and never shares it — so there is exactly one block instance
/// per module instance, and no cross-job state that could leak between them.
///
/// Every export takes and returns `usize` rather than a packed integer. On
/// `wasm32` that lowers to `i32` parameters and results; on a 64-bit guest the
/// same source compiles to `i64` with nothing here changing.
#[macro_export]
macro_rules! export_block {
    ($ty:ty) => {
        thread_local! {
            static __CF_STATE: ::std::cell::RefCell<$ty> =
                ::std::cell::RefCell::new(<$ty as ::core::default::Default>::default());
        }

        #[no_mangle]
        pub extern "C" fn cf_alloc(len: usize) -> usize {
            $crate::__alloc(len)
        }

        /// Report this block's type signature. Read at build time, before any
        /// job runs, to check that a pipeline's seams line up.
        #[no_mangle]
        pub extern "C" fn cf_signature() -> usize {
            $crate::__write_json(&<$ty as $crate::Block>::signature())
        }

        #[no_mangle]
        pub extern "C" fn cf_init(ptr: usize, len: usize) -> usize {
            let input: ::serde_json::Value = unsafe { $crate::__read_json(ptr, len) };
            let cmd = __CF_STATE.with(|s| $crate::Block::start(&mut *s.borrow_mut(), input));
            $crate::__write_json(&cmd)
        }

        #[no_mangle]
        pub extern "C" fn cf_step(ptr: usize, len: usize) -> usize {
            let event: $crate::Event = unsafe { $crate::__read_json(ptr, len) };
            let cmd = __CF_STATE.with(|s| $crate::Block::step(&mut *s.borrow_mut(), event));
            $crate::__write_json(&cmd)
        }

        #[no_mangle]
        pub extern "C" fn cf_on_token(ptr: usize, len: usize) -> i32 {
            // Lossy on purpose: a model can emit a token split mid-character,
            // and mangling one character is a far better outcome than trapping
            // the whole job over it.
            let token: ::std::string::String = unsafe {
                let slice = ::std::slice::from_raw_parts(ptr as *const u8, len);
                ::std::string::String::from_utf8_lossy(slice).into_owned()
            };
            $crate::TokenAction::as_i32(
                __CF_STATE.with(|s| $crate::Block::on_token(&mut *s.borrow_mut(), &token)),
            )
        }
    };
}

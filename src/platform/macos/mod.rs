//! macOS backend: an event tap on its own thread, plus a supervisor thread that manages the Accessibility permission lifecycle.
//!
//! The supervisor is a separate thread because the tap thread runs the user callback.
//! If that callback blocks, the supervisor can still check the tap and invalidate it.

mod convert;
pub(crate) mod keycode;
mod lifecycle;
mod stream;
mod supervisor;
mod tap;
mod tcc;

pub(crate) use supervisor::Supervisor;

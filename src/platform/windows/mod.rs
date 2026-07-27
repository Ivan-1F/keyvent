//! Windows backend: a low-level keyboard hook (`WH_KEYBOARD_LL`) on a
//! message-pump thread.

pub(crate) mod convert;
pub(crate) mod keycode;

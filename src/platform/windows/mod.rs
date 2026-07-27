//! Windows backend: one low-level keyboard hook thread, no supervisor.

#[cfg(any(target_os = "windows", test))]
mod convert;
#[cfg(target_os = "windows")]
mod hook;
#[cfg(any(target_os = "windows", test))]
mod keycode;

#[cfg(target_os = "windows")]
pub(crate) use hook::HookThread;

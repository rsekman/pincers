//! This crate is the library for `pince.rs`, a Wayland clipboard manager based on Vim's registers.
//! The documentation here covers the internal workings of the clipboard manager. For how to use
//! `pince.rs`, refer to the manpage

/// Client-side
pub mod client;
/// Interact with the Wayland compositor
pub mod clipboard;
/// Manage IPC
pub mod daemon;
/// Error handling
pub mod error;
/// A [`Pincer`](pincer::Pincer) manages several registers.
pub mod pincer;
/// A [`Register`](register::Register) is a map from MIME types to data buffers
pub mod register;
/// Manage data related to a Wayland seat
pub mod seat;

//! Presenter abstraction: one drawing API, two backends.
//!
//! The real target is DRM/KMS on a Raspberry Pi 3 with no compositor ([`kms`]). But DRM
//! requires DRM master, which a running desktop session already holds, so UI code cannot be
//! iterated on the dev machine against a real KMS surface. [`desktop`] provides a
//! winit/glutin window with an otherwise identical drawing path, so everything above
//! [`Presenter`] is written once.

pub mod presenter;

#[cfg(feature = "kms")]
pub mod kms;

#[cfg(feature = "offscreen")]
pub mod offscreen;

#[cfg(feature = "desktop")]
pub mod desktop;

pub use presenter::{Canvas, GlInfo, Presenter, Pump};

// Re-exported so downstream crates don't need to depend on femtovg directly and can't
// accidentally pick a different version than the one the presenters were built against.
pub use femtovg;

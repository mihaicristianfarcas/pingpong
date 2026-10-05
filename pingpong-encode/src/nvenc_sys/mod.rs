//! Raw NVENC API 12.1 types, vendored from `nvidia-video-codec-sdk` 0.4.0
//! (MIT, bindgen output of NVIDIA's MIT-licensed `nvEncodeAPI.h`; both
//! notices are in `LICENSE` beside this file).
//!
//! Types only. The function table is filled at runtime from
//! `nvEncodeAPI64.dll`, which ships with the driver, exactly as Sunshine does --
//! so building the host needs neither the Video Codec SDK nor an import library.

#![allow(warnings)]
#![allow(clippy::all)]

mod guid;
mod version;
#[rustfmt::skip]
mod api;

pub use api::*;

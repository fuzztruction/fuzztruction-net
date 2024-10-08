#![allow(clippy::all)]
#![feature(naked_functions)]
#![feature(new_uninit)]
// memoffset library
#![feature(allocator_api)]

extern crate memoffset;
pub mod agent;
pub mod mem;
pub mod tracing;

pub mod allocator;
pub mod jit;
pub mod logging;

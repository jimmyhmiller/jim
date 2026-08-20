//! Reads a frame out of an IOSurface published by the host process.
//!
//! The surface is found by id — the only thing that crossed the socket — and
//! its pixels are already in memory both processes can see.
//!
//! This copies into the `bevy::Image`, which is the one copy still left in
//! the pipeline. Removing it means substituting the `GpuImage` behind the
//! image handle in the render world with a texture imported straight from
//! this IOSurface (the Metal import is already proven); that is the next
//! optimisation, not a rewrite.

use objc2_io_surface::{IOSurfaceLockOptions, IOSurfaceRef};

pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// Look up a shared surface by id and copy its BGRA contents out.
pub fn read(id: u32) -> Option<Frame> {
    let surface = IOSurfaceRef::lookup(id)?;
    let width = surface.width() as u32;
    let height = surface.height() as u32;
    if width == 0 || height == 0 {
        return None;
    }

    let row = (width as usize) * 4;
    let mut bgra = vec![0u8; row * height as usize];
    unsafe {
        // Read-only lock: we are a consumer, the host keeps painting.
        surface.lock(IOSurfaceLockOptions::ReadOnly, std::ptr::null_mut());
        let base = surface.base_address().as_ptr() as *const u8;
        let stride = surface.bytes_per_row();
        for y in 0..height as usize {
            std::ptr::copy_nonoverlapping(
                base.add(y * stride),
                bgra.as_mut_ptr().add(y * row),
                row.min(stride),
            );
        }
        surface.unlock(IOSurfaceLockOptions::ReadOnly, std::ptr::null_mut());
    }

    Some(Frame {
        width,
        height,
        bgra,
    })
}

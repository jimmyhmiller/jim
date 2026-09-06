//! A globally-shareable IOSurface that CEF's painted pixels are copied into.
//!
//! CEF's own accelerated-OSR surfaces are shared through mach ports rather
//! than being globally lookupable, so rather than plumb mach ports we own one
//! surface per browser and copy each painted frame into it. The copy is a
//! memcpy inside this process; nothing is copied across the process boundary,
//! which is the part that would actually cost bandwidth.

use std::ffi::c_void;

use objc2_core_foundation::{
    kCFBooleanTrue, kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFBoolean,
    CFDictionary, CFNumber, CFRetained, CFString,
};
use objc2_io_surface::{IOSurfaceLockOptions, IOSurfaceRef};

pub struct SharedSurface {
    pub surface: CFRetained<IOSurfaceRef>,
    pub width: u32,
    pub height: u32,
}

impl SharedSurface {
    pub fn new(width: u32, height: u32) -> Option<Self> {
        let w = CFNumber::new_i32(width as i32);
        let h = CFNumber::new_i32(height as i32);
        let bpe = CFNumber::new_i32(4);
        // CEF paints BGRA.
        let fmt = CFNumber::new_i32(i32::from_be_bytes(*b"BGRA"));

        let props = unsafe {
            let global = kCFBooleanTrue?;
            let mut keys: [*const c_void; 5] = [
                (objc2_io_surface::kIOSurfaceWidth as *const CFString).cast(),
                (objc2_io_surface::kIOSurfaceHeight as *const CFString).cast(),
                (objc2_io_surface::kIOSurfaceBytesPerElement as *const CFString).cast(),
                (objc2_io_surface::kIOSurfacePixelFormat as *const CFString).cast(),
                // Without this the surface cannot be found by id from jim.
                (objc2_io_surface::kIOSurfaceIsGlobal as *const CFString).cast(),
            ];
            let mut vals: [*const c_void; 5] = [
                (&*w as *const CFNumber).cast(),
                (&*h as *const CFNumber).cast(),
                (&*bpe as *const CFNumber).cast(),
                (&*fmt as *const CFNumber).cast(),
                (global as *const CFBoolean).cast(),
            ];
            CFDictionary::new(
                None,
                keys.as_mut_ptr(),
                vals.as_mut_ptr(),
                5,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            )?
        };

        let surface = unsafe { IOSurfaceRef::new(&props) }?;
        Some(Self {
            surface,
            width,
            height,
        })
    }

    pub fn id(&self) -> u32 {
        self.surface.id()
    }

    /// Copy a tightly-packed BGRA frame in, respecting the surface stride.
    pub fn write_bgra(&self, src: &[u8], width: u32, height: u32) {
        if width != self.width || height != self.height {
            return;
        }
        unsafe {
            self.surface
                .lock(IOSurfaceLockOptions::empty(), std::ptr::null_mut());
            let base = self.surface.base_address().as_ptr() as *mut u8;
            let stride = self.surface.bytes_per_row();
            let row = (width as usize) * 4;
            for y in 0..height as usize {
                std::ptr::copy_nonoverlapping(
                    src.as_ptr().add(y * row),
                    base.add(y * stride),
                    row.min(stride),
                );
            }
            self.surface
                .unlock(IOSurfaceLockOptions::empty(), std::ptr::null_mut());
        }
    }
}

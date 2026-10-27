//! whatsapp-gpui patch: `GPUI_FRAME_STATS=<file>` appends one line per frame for profiling:
//! `F,<submitted>,<encode µs>,<gpu start>,<gpu end>,<panes redrawn>` once the GPU finishes, and
//! `P,<presented>` when the main drawable reaches the screen (0 if it was dropped).
//! Times are host seconds (`CACurrentMediaTime`).

use super::{AnyObject, Mutex, RcBlock, ptr};
use foreign_types::ForeignTypeRef;
use std::{fs::File, io::Write, sync::OnceLock};

static FILE: OnceLock<Option<Mutex<File>>> = OnceLock::new();

/// `CACurrentMediaTime`: host time in seconds (no `extern` block here: cbindgen scans this
/// module into the shader header).
#[allow(deprecated)]
fn host_seconds() -> f64 {
    static TIMEBASE: OnceLock<(u32, u32)> = OnceLock::new();
    let (numer, denom) = *TIMEBASE.get_or_init(|| {
        let mut info = libc::mach_timebase_info { numer: 0, denom: 0 };
        unsafe { libc::mach_timebase_info(&mut info) };
        (info.numer, info.denom)
    });
    let ticks = unsafe { libc::mach_absolute_time() };
    ticks as f64 * numer as f64 / denom as f64 / 1e9
}

fn file() -> Option<&'static Mutex<File>> {
    FILE.get_or_init(|| {
        let path = std::env::var_os("GPUI_FRAME_STATS")?;
        File::create(path).ok().map(Mutex::new)
    })
    .as_ref()
}

pub(super) fn enabled() -> bool {
    file().is_some()
}

fn write(line: String) {
    if let Some(f) = file() {
        let _ = f.lock().write_all(line.as_bytes());
    }
}

pub(super) fn record(
    command_buffer: &metal::CommandBufferRef,
    drawable: &metal::MetalDrawableRef,
    start: std::time::Instant,
    panes: usize,
) {
    let submitted = host_seconds();
    let encode_us = start.elapsed().as_micros();
    let completed = RcBlock::new(move |buffer: ptr::NonNull<AnyObject>| {
        let buffer = buffer.as_ptr();
        let (gpu_start, gpu_end): (f64, f64) = unsafe {
            (objc2::msg_send![buffer, GPUStartTime], objc2::msg_send![buffer, GPUEndTime])
        };
        write(format!("F,{submitted:.6},{encode_us},{gpu_start:.6},{gpu_end:.6},{panes}\n"));
    });
    let presented = RcBlock::new(move |drawable: ptr::NonNull<AnyObject>| {
        let at: f64 = unsafe { objc2::msg_send![drawable.as_ptr(), presentedTime] };
        write(format!("P,{at:.6}\n"));
    });
    unsafe {
        command_buffer.add_completed_handler(&*RcBlock::as_ptr(&completed).cast());
        let _: () = objc2::msg_send![drawable.as_ptr().cast::<AnyObject>(), addPresentedHandler: &*presented];
    }
}

//! DEBUG-ONLY crash tracer: a vectored exception handler that prints the
//! faulting address and the module chain of the stack before the process
//! dies. The release build never compiles this.

#![cfg(debug_assertions)]

use windows::core::PCWSTR;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::Diagnostics::Debug::{AddVectoredExceptionHandler, EXCEPTION_POINTERS};
use windows::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};

/// Logs the first hard access violation with its stack, then steps aside.
pub fn install() {
    unsafe {
        AddVectoredExceptionHandler(1, Some(on_exception));
    }
}

unsafe extern "system" fn on_exception(info: *mut EXCEPTION_POINTERS) -> i32 {
    let ep: &EXCEPTION_POINTERS = &*info;
    let rec = &*ep.ExceptionRecord;
    let code = rec.ExceptionCode.0 as u32;
    // Only hard access violations: breakpoints and C++/Rust unwind exceptions
    // pass through untouched.
    if code != 0xC000_0005 {
        return 0;
    }
    let addr = rec.ExceptionAddress as usize;

    let module_of = |addr: usize| -> (String, usize) {
        let mut hmod = HMODULE::default();
        if GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(addr as *const u16),
            &mut hmod,
        )
        .is_ok()
        {
            let mut buf = [0u16; 260];
            let n = GetModuleFileNameW(Some(hmod), &mut buf) as usize;
            let path = String::from_utf16_lossy(&buf[..n]);
            let base = hmod.0 as usize;
            let name = path.rsplit(['\\', '/']).next().unwrap_or("?").to_string();
            return (name, addr - base);
        }
        ("?".to_string(), addr)
    };

    let (m, off) = module_of(addr);
    eprintln!("CRASH: code={code:#x} at {m}+{off:#x}");
    if rec.NumberParameters >= 2 {
        let kind = if rec.ExceptionInformation[0] == 0 {
            "read"
        } else {
            "write"
        };
        eprintln!(
            "CRASH: {kind} of address {:#x}",
            rec.ExceptionInformation[1]
        );
    }

    let bt = std::backtrace::Backtrace::force_capture();
    eprintln!("CRASH backtrace:\n{bt}");

    0 // EXCEPTION_CONTINUE_SEARCH: the process still dies
}

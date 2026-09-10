//! The console window an installed IRA should not leave lying around.
//!
//! IRA is a console program and has to stay one: `ira doctor`, `ira set` and
//! `ira fetch` are how she is configured, and a `windows_subsystem = "windows"`
//! binary run from PowerShell returns to the prompt immediately and then prints
//! over it. The usual fix for that -- declare the GUI subsystem and call
//! `AttachConsole` -- buys a hidden window at the cost of every command in this
//! program printing after the shell has moved on.
//!
//! So the subsystem stays as it is and the window goes the other way. Windows
//! gives a process launched from a shortcut a console of its very own, and one
//! launched from a terminal a console it *shares* with the shell. Those two are
//! distinguishable: `GetConsoleProcessList` reports how many processes are
//! attached, and the answer is 1 only when nobody else is there.
//!
//! Alone means the window belongs to IRA and closing it costs nothing.
//! Sharing means it is the user's terminal, and IRA touching it would take a
//! shell's window away mid-session.
//!
//! Freeing it late, rather than at start-up, is what keeps a first run
//! watchable: the models are 85 MB, and a fresh install that downloads them
//! behind a window that has already vanished looks like one that has hung.
//!
//! Standard error is pointed at a log file first, because a program with no
//! console has nowhere to say anything -- and the run where that matters most
//! is exactly the one nobody is watching.

use std::path::Path;

/// Whether this console is IRA's alone, and can be closed without taking
/// somebody's shell with it.
///
/// False anywhere there is no console at all, which is the right answer for
/// both halves of that question.
pub fn owned_alone() -> bool {
    use windows_sys::Win32::System::Console::GetConsoleProcessList;
    // Two slots: the count is all that matters, and asking for one process id
    // when there are two returns an error rather than the count.
    let mut pids = [0u32; 2];
    let n = unsafe { GetConsoleProcessList(pids.as_mut_ptr(), pids.len() as u32) };
    n == 1
}

/// Points standard output and error at `log`, then closes the console.
///
/// Best effort throughout: every failure here leaves IRA running with a console
/// she meant to close, which is untidy and nothing worse. Failing the start-up
/// over it would turn a cosmetic problem into an outage.
pub fn detach_into(log: &Path) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_APPEND_DATA, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_ALWAYS,
    };
    use windows_sys::Win32::System::Console::{
        FreeConsole, SetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
    };

    let wide: Vec<u16> = log.as_os_str().encode_wide().chain(Some(0)).collect();
    // Appended rather than truncated: the run before this one is often the one
    // that explains the run being looked at.
    //
    // ponytail: nothing rotates this. It grows by a few lines a session; the
    // day it does not, give it a size check and a single `.old` rename.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_WRITE | FILE_APPEND_DATA,
            // Shared, so a second IRA and a person tailing the file can both
            // have it open. Neither is normal; neither should fail.
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle != INVALID_HANDLE_VALUE && !handle.is_null() {
        // Rust asks for the handle on every write rather than caching one at
        // start-up, which is what makes swapping it underneath work at all.
        unsafe {
            SetStdHandle(STD_OUTPUT_HANDLE, handle);
            SetStdHandle(STD_ERROR_HANDLE, handle);
        }
    }
    unsafe {
        FreeConsole();
    }
}

/// Prints a failure and waits, so a window that is about to close does not take
/// the reason with it.
///
/// Only ever reached on a shortcut launch that failed before the console was
/// closed. From a terminal the shell keeps the output and this is not called;
/// once IRA is running there is nothing left that can fail this way.
///
/// A message box would not need the console at all, but it also cannot be
/// copied into a bug report, scrolled, or piped anywhere. The console is still
/// open at this point and it already holds the log lines leading up to the
/// failure, which are usually the more useful half.
pub fn hold(problem: &str) {
    eprintln!();
    eprintln!("IRA could not start.");
    eprintln!();
    eprintln!("{problem}");
    eprintln!();
    eprintln!("Press Enter to close this window.");
    let mut discard = String::new();
    let _ = std::io::stdin().read_line(&mut discard);
}

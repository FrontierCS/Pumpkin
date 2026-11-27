//! The process's environment, which `std` lets a process change only while it runs one thread.

use std::ffi::OsString;
use std::{env, fs, io};

/// The values of the environment variables `names`, each `None` where it is unset, which this
/// removes from the process's environment. No program that the process starts later inherits one.
///
/// # Panics
///
/// Panics if the process runs another thread: take the variables before it starts any.
#[expect(
    unsafe_code,
    reason = "`std` makes removing a variable `unsafe`, as another thread may read the environment \
              meanwhile"
)]
pub fn take<const N: usize>(names: [&str; N]) -> io::Result<[Option<OsString>; N]> {
    let threads = fs::read_dir("/proc/self/task")?.count();
    assert!(
        threads == 1,
        "the process runs {threads} threads while it takes variables from its environment: take \
         them before it starts another"
    );
    Ok(names.map(|name| {
        let value = env::var_os(name);
        // SAFETY: `remove_var` needs that no other thread reads or writes the environment
        // meanwhile. No other thread exists: the process ran one, this one, as its tasks show,
        // and only this thread could start another.
        unsafe { env::remove_var(name) };
        value
    }))
}

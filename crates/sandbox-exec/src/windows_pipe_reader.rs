//! P2/TASK-202：并发排空 Windows 子进程输出管道，避免缓冲区死锁。

use std::{
    ffi::c_void,
    fs::File,
    io::{self, Read},
    os::windows::io::FromRawHandle,
    thread,
};

pub(crate) fn reader_thread(raw_handle: usize) -> thread::JoinHandle<io::Result<Vec<u8>>> {
    thread::spawn(move || {
        // SAFETY: pipe handle ownership was transferred from the Win32 owner exactly once.
        let mut file = unsafe { File::from_raw_handle(raw_handle as *mut c_void) };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    })
}

pub(crate) fn join_reader(reader: thread::JoinHandle<io::Result<Vec<u8>>>) -> io::Result<Vec<u8>> {
    reader
        .join()
        .map_err(|_| io::Error::other("pipe reader thread panicked"))?
}

//! moruna: stdout and stderr are lines of the task's log, a record each (`moruna_sys::rt::stdio`);
//! stdin is empty.

use crate::io;

pub const STDIN_BUF_SIZE: usize = 0;

pub struct Stdin;
pub struct Stdout;
pub type Stderr = Stdout;

impl Stdin {
    pub const fn new() -> Stdin {
        Stdin
    }
}

impl io::Read for Stdin {
    fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
        Ok(0)
    }
}

impl Stdout {
    pub const fn new() -> Stdout {
        Stdout
    }
}

impl io::Write for Stdout {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        Ok(moruna_sys::rt::stdio::write(buf))
    }

    /// A line is in the log when its newline is written; a flush has nothing to add.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn is_ebadf(_err: &io::Error) -> bool {
    true
}

pub fn panic_output() -> Option<impl io::Write> {
    Some(Stdout::new())
}

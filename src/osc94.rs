use std::io::{Stderr, Write};

pub struct OSC94 {
    stderr: Stderr,
}

impl OSC94 {
    pub fn new() -> Self {
        Self {
            stderr: std::io::stderr(),
        }
    }
    pub fn progress<T: TryInto<u8>>(&mut self, percent: Option<T>)
    where
        <T as std::convert::TryInto<u8>>::Error: std::fmt::Debug,
    {
        if let Some(percent) = percent {
            self.stderr.write_all(b"\x1b]9;4;1;").ok();
            self.stderr
                .write_all(
                    percent
                        .try_into()
                        .expect("failed to cast percent")
                        .to_string()
                        .as_bytes(),
                )
                .ok();
            self.stderr.write_all(b"\x07").ok();
        } else {
            self.stderr.write_all(b"\x1b]9;4;3;0\x07").ok();
        }
    }
}

impl Drop for OSC94 {
    fn drop(&mut self) {
        // Clear the progress bar when the OSC94 struct is dropped.
        self.stderr.write_all(b"\x1b]9;4;0;0\x07").ok();
    }
}

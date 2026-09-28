//! The compositor's XKB keymap, as received in `wl_keyboard.keymap`.
//!
//! smithay sends every client the same sealed memfd (one open file description, so a shared
//! file offset), with a trailing NUL counted in `size`. We therefore only `pread` from offset 0
//! and never `read`/`lseek` it, and keep our own copy of the bytes. The EIS side (Phase 2)
//! hands KDE Connect a fresh memfd made from this copy; kdeconnectd crashes on a keyboard
//! device without a keymap, and parses the text with `xkb_keymap_new_from_string`, which needs
//! the NUL within `size`.

use std::os::fd::OwnedFd;

use anyhow::{Result, bail};

#[derive(Clone, PartialEq, Eq)]
pub struct Keymap {
    /// Keymap text including the trailing NUL.
    bytes: Vec<u8>,
}

impl Keymap {
    pub fn from_wayland(fd: OwnedFd, size: u32) -> Result<Self> {
        if size == 0 {
            bail!("keymap size is 0");
        }
        let size = size as usize;
        let mut bytes = vec![0u8; size];
        let mut off = 0;
        while off < size {
            let n = rustix::io::pread(&fd, &mut bytes[off..], off as u64)?;
            if n == 0 {
                bail!("keymap fd ended after {off} of {size} bytes");
            }
            off += n;
        }
        if bytes.last() != Some(&0) {
            bail!("keymap is not NUL-terminated within its size");
        }
        Ok(Self { bytes })
    }

    /// Size including the trailing NUL, as it must be announced over EIS.
    pub fn size(&self) -> usize {
        self.bytes.len()
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The keymap text without the trailing NUL.
    pub fn text(&self) -> std::borrow::Cow<'_, str> {
        let end = self.bytes.iter().position(|&b| b == 0).unwrap_or(self.bytes.len());
        String::from_utf8_lossy(&self.bytes[..end])
    }

    pub fn first_line(&self) -> String {
        self.text().lines().next().unwrap_or_default().to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::fs::{MemfdFlags, memfd_create};

    fn memfd_with(data: &[u8]) -> OwnedFd {
        let fd = memfd_create("test", MemfdFlags::CLOEXEC).unwrap();
        rustix::io::write(&fd, data).unwrap();
        fd
    }

    #[test]
    fn reads_from_offset_zero_regardless_of_file_position() {
        let fd = memfd_with(b"xkb_keymap {\n};\n\0");
        // The shared file offset is now at the end; pread must not care.
        let km = Keymap::from_wayland(fd, 17).unwrap();
        assert_eq!(km.size(), 17);
        assert_eq!(km.first_line(), "xkb_keymap {");
    }

    #[test]
    fn rejects_missing_nul_and_zero_size() {
        assert!(Keymap::from_wayland(memfd_with(b"abc"), 3).is_err());
        assert!(Keymap::from_wayland(memfd_with(b""), 0).is_err());
        assert!(Keymap::from_wayland(memfd_with(b"ab\0"), 10).is_err());
    }
}

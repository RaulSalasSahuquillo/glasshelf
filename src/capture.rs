//! Screen-capture helpers (wlr-screencopy): shared-memory buffers and pixel-format
//! conversion to RGBA8.
use anyhow::{anyhow, Result};
use memmap2::{MmapMut, MmapOptions};
use rustix::fs::{ftruncate, memfd_create, MemfdFlags};
use std::os::fd::OwnedFd;
use wayland_client::protocol::wl_shm;

pub struct ShmBuf {
    pub fd: OwnedFd,
    pub map: MmapMut,
}

pub fn alloc(size: usize) -> Result<ShmBuf> {
    let fd = memfd_create("glassdock-capture", MemfdFlags::CLOEXEC)?;
    ftruncate(&fd, size as u64)?;
    let map = unsafe { MmapOptions::new().len(size).map_mut(&fd) }.map_err(|e| anyhow!("mmap: {e}"))?;
    Ok(ShmBuf { fd, map })
}

/// Converts the contents of a shm buffer to RGBA8 (alpha = 255), top row first.
pub fn to_rgba(fmt: wl_shm::Format, w: usize, h: usize, stride: usize, data: &[u8], y_invert: bool) -> Option<Vec<u8>> {
    if data.len() < stride * h {
        return None;
    }
    let mut out = vec![255u8; w * h * 4];
    for y in 0..h {
        let src_y = if y_invert { h - 1 - y } else { y };
        let row = &data[src_y * stride..src_y * stride + w * 4];
        let dst = &mut out[y * w * 4..(y + 1) * w * 4];
        match fmt {
            wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888 => {
                for x in 0..w {
                    dst[x * 4] = row[x * 4 + 2];
                    dst[x * 4 + 1] = row[x * 4 + 1];
                    dst[x * 4 + 2] = row[x * 4];
                }
            }
            wl_shm::Format::Abgr8888 | wl_shm::Format::Xbgr8888 => {
                for x in 0..w {
                    dst[x * 4] = row[x * 4];
                    dst[x * 4 + 1] = row[x * 4 + 1];
                    dst[x * 4 + 2] = row[x * 4 + 2];
                }
            }
            wl_shm::Format::Xrgb2101010 | wl_shm::Format::Argb2101010 => {
                for x in 0..w {
                    let v = u32::from_le_bytes([row[x * 4], row[x * 4 + 1], row[x * 4 + 2], row[x * 4 + 3]]);
                    dst[x * 4] = ((v >> 22) & 0xff) as u8;
                    dst[x * 4 + 1] = ((v >> 12) & 0xff) as u8;
                    dst[x * 4 + 2] = ((v >> 2) & 0xff) as u8;
                }
            }
            wl_shm::Format::Xbgr2101010 | wl_shm::Format::Abgr2101010 => {
                for x in 0..w {
                    let v = u32::from_le_bytes([row[x * 4], row[x * 4 + 1], row[x * 4 + 2], row[x * 4 + 3]]);
                    dst[x * 4] = ((v >> 2) & 0xff) as u8;
                    dst[x * 4 + 1] = ((v >> 12) & 0xff) as u8;
                    dst[x * 4 + 2] = ((v >> 22) & 0xff) as u8;
                }
            }
            _ => return None,
        }
    }
    Some(out)
}

/// Mean luminance (0..1) of the lower part of the image (the panel area), subsampled.
pub fn mean_luma(rgba: &[u8], w: usize, h: usize) -> f32 {
    let y0 = (h as f32 * 0.40) as usize;
    let mut sum = 0.0f64;
    let mut n = 0u64;
    let mut y = y0;
    while y < h {
        let mut x = 0;
        while x < w {
            let i = (y * w + x) * 4;
            sum += 0.2126 * rgba[i] as f64 + 0.7152 * rgba[i + 1] as f64 + 0.0722 * rgba[i + 2] as f64;
            n += 1;
            x += 4;
        }
        y += 4;
    }
    if n == 0 {
        0.3
    } else {
        (sum / n as f64 / 255.0) as f32
    }
}

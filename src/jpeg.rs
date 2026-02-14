//! FFI bindings and safe wrapper for esp_new_jpeg block-mode decoder.

use alloc::alloc::{Layout, alloc, dealloc};
use core::ffi::c_void;
use core::ptr;

// ---------------------------------------------------------------------------
// JPEG pixel format FOURCC constants
// ---------------------------------------------------------------------------

const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}

pub const JPEG_PIXEL_FORMAT_RGB565_LE: u32 = fourcc(b'R', b'G', b'B', b'L');

// ---------------------------------------------------------------------------
// Repr(C) structs matching the C header definitions
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct JpegResolution {
    pub width: u16,
    pub height: u16,
}

#[repr(C)]
pub struct JpegDecConfig {
    pub output_type: u32,
    pub scale: JpegResolution,
    pub clipper: JpegResolution,
    pub rotate: u32,
    pub block_enable: bool,
}

#[repr(C)]
pub struct JpegDecIo {
    pub inbuf: *mut u8,
    pub inbuf_len: i32,
    pub inbuf_remain: i32,
    pub outbuf: *mut u8,
    pub out_size: i32,
}

#[repr(C)]
pub struct JpegDecHeaderInfo {
    pub width: u16,
    pub height: u16,
}

// ---------------------------------------------------------------------------
// FFI declarations
// ---------------------------------------------------------------------------

type JpegDecHandle = *mut c_void;

unsafe extern "C" {
    fn jpeg_dec_open(config: *mut JpegDecConfig, handle: *mut JpegDecHandle) -> i32;
    fn jpeg_dec_parse_header(
        handle: JpegDecHandle,
        io: *mut JpegDecIo,
        info: *mut JpegDecHeaderInfo,
    ) -> i32;
    fn jpeg_dec_get_outbuf_len(handle: JpegDecHandle, outbuf_len: *mut i32) -> i32;
    fn jpeg_dec_get_process_count(handle: JpegDecHandle, count: *mut i32) -> i32;
    fn jpeg_dec_process(handle: JpegDecHandle, io: *mut JpegDecIo) -> i32;
    fn jpeg_dec_close(handle: JpegDecHandle) -> i32;

    fn jpeg_calloc_align(size: usize, aligned: i32) -> *mut c_void;
    fn jpeg_free_align(data: *mut c_void);
}

// ---------------------------------------------------------------------------
// ESP-IDF heap_caps stubs — the .a library calls these internally
//
// Strategy: allocate with Rust's global allocator using Layout(size, align=1),
// store (raw_ptr, alloc_size) in a header just before the returned pointer.
// ---------------------------------------------------------------------------

const META_WORDS: usize = 2; // raw_ptr + alloc_size
const META_BYTES: usize = META_WORDS * core::mem::size_of::<usize>();

unsafe fn caps_alloc_inner(count: usize, size: usize, align: usize) -> *mut u8 {
    let payload = match count.checked_mul(size) {
        Some(0) | None => return ptr::null_mut(),
        Some(p) => p,
    };
    let align = align.max(core::mem::size_of::<usize>());
    let alloc_size = payload + META_BYTES + align;
    unsafe {
        let layout = Layout::from_size_align_unchecked(alloc_size, 1);
        let raw = alloc(layout);
        if raw.is_null() {
            return ptr::null_mut();
        }
        raw.write_bytes(0, alloc_size);
        // Aligned user pointer with room for header
        let base = raw as usize + META_BYTES;
        let user_addr = (base + align - 1) & !(align - 1);
        let meta = user_addr as *mut usize;
        meta.sub(1).write(raw as usize);
        meta.sub(2).write(alloc_size);
        user_addr as *mut u8
    }
}

unsafe fn caps_free_inner(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    unsafe {
        let meta = ptr as *mut usize;
        let raw = meta.sub(1).read() as *mut u8;
        let alloc_size = meta.sub(2).read();
        let layout = Layout::from_size_align_unchecked(alloc_size, 1);
        dealloc(raw, layout);
    }
}

/// `void *heap_caps_calloc_prefer(size_t n, size_t size, size_t num, ...)`
/// The variadic caps arguments are ignored — we just allocate from Rust's heap.
#[unsafe(no_mangle)]
pub extern "C" fn heap_caps_calloc_prefer(n: usize, size: usize, _num: usize) -> *mut c_void {
    unsafe { caps_alloc_inner(n, size, 4) as *mut c_void }
}

/// `void heap_caps_free(void *ptr)`
#[unsafe(no_mangle)]
pub extern "C" fn heap_caps_free(ptr: *mut c_void) {
    unsafe { caps_free_inner(ptr as *mut u8) }
}

/// `void *heap_caps_aligned_calloc(size_t alignment, size_t n, size_t size, uint32_t caps)`
#[unsafe(no_mangle)]
pub extern "C" fn heap_caps_aligned_calloc(
    alignment: usize,
    n: usize,
    size: usize,
    _caps: u32,
) -> *mut c_void {
    unsafe { caps_alloc_inner(n, size, alignment) as *mut c_void }
}

// ---------------------------------------------------------------------------
// ESP-IDF logging stubs — the .a library calls these for diagnostics
// ---------------------------------------------------------------------------

/// `void esp_log_write(esp_log_level_t level, const char *tag, const char *format, ...)`
#[unsafe(no_mangle)]
pub extern "C" fn esp_log_write(_level: u32, _tag: *const u8, _fmt: *const u8) {
    // no-op: variadic printf formatting is not practical in no_std
}

/// `void esp_log_level_set(const char *tag, esp_log_level_t level)`
#[unsafe(no_mangle)]
pub extern "C" fn esp_log_level_set(_tag: *const u8, _level: u32) {}

/// `uint32_t esp_log_timestamp(void)`
#[unsafe(no_mangle)]
pub extern "C" fn esp_log_timestamp() -> u32 {
    0
}

// ---------------------------------------------------------------------------
// Safe wrapper
// ---------------------------------------------------------------------------

pub struct JpegDecoder {
    handle: JpegDecHandle,
}

pub struct JpegFrameInfo {
    pub width: u16,
    pub height: u16,
}

impl JpegDecoder {
    /// Create a new block-mode JPEG decoder with RGB565_LE output.
    pub fn new() -> Result<Self, i32> {
        let mut config = JpegDecConfig {
            output_type: JPEG_PIXEL_FORMAT_RGB565_LE,
            scale: JpegResolution {
                width: 0,
                height: 0,
            },
            clipper: JpegResolution {
                width: 0,
                height: 0,
            },
            rotate: 0, // JPEG_ROTATE_0D
            block_enable: true,
        };
        let mut handle: JpegDecHandle = ptr::null_mut();
        let ret = unsafe { jpeg_dec_open(&mut config, &mut handle) };
        if ret != 0 {
            return Err(ret);
        }
        Ok(Self { handle })
    }

    /// Decode a complete JPEG frame, calling `on_block` for each decoded strip.
    ///
    /// `on_block(block_index, width, height, rgb565_le_data)` receives the block
    /// index, dimensions of the decoded strip, and the raw RGB565-LE pixel bytes.
    pub fn decode<F>(
        &mut self,
        jpeg_data: &mut [u8],
        mut on_block: F,
    ) -> Result<JpegFrameInfo, i32>
    where
        F: FnMut(usize, u16, u16, &[u8]),
    {
        let mut io = JpegDecIo {
            inbuf: jpeg_data.as_mut_ptr(),
            inbuf_len: jpeg_data.len() as i32,
            inbuf_remain: 0,
            outbuf: ptr::null_mut(),
            out_size: 0,
        };

        // Parse header
        let mut header = JpegDecHeaderInfo {
            width: 0,
            height: 0,
        };
        let ret = unsafe { jpeg_dec_parse_header(self.handle, &mut io, &mut header) };
        if ret != 0 {
            return Err(ret);
        }

        let info = JpegFrameInfo {
            width: header.width,
            height: header.height,
        };

        // Get output buffer size for one block
        let mut outbuf_len: i32 = 0;
        let ret = unsafe { jpeg_dec_get_outbuf_len(self.handle, &mut outbuf_len) };
        if ret != 0 {
            return Err(ret);
        }

        // Allocate 16-byte aligned output buffer via the library's own allocator
        let outbuf = unsafe { jpeg_calloc_align(outbuf_len as usize, 16) };
        if outbuf.is_null() {
            return Err(-2); // JPEG_ERR_NO_MEM
        }
        io.outbuf = outbuf as *mut u8;

        // Get number of blocks to process
        let mut process_count: i32 = 0;
        let ret = unsafe { jpeg_dec_get_process_count(self.handle, &mut process_count) };
        if ret != 0 {
            unsafe { jpeg_free_align(outbuf) };
            return Err(ret);
        }

        // Decode block by block
        for i in 0..process_count as usize {
            io.out_size = 0;
            let ret = unsafe { jpeg_dec_process(self.handle, &mut io) };
            if ret != 0 {
                unsafe { jpeg_free_align(outbuf) };
                return Err(ret);
            }

            let block_data =
                unsafe { core::slice::from_raw_parts(io.outbuf, io.out_size as usize) };

            // Calculate block height: out_size / (width * 2 bytes per pixel)
            let block_width = header.width;
            let block_height = if block_width > 0 {
                (io.out_size as u16) / (block_width * 2)
            } else {
                0
            };

            on_block(i, block_width, block_height, block_data);
        }

        unsafe { jpeg_free_align(outbuf) };
        Ok(info)
    }
}

impl Drop for JpegDecoder {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { jpeg_dec_close(self.handle) };
        }
    }
}

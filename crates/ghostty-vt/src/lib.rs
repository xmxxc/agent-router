//! Terminal operations used by the WFC stdio kernel. Adapted from Herdr 0.9.5 (Apache-2.0).
#[allow(
    dead_code,
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    clippy::all,
    rustdoc::all
)]
pub mod bindings;

use std::ffi::c_void;
use std::{fmt, mem, ptr, slice};

/// Terminfo entry the terminal emulates; child processes should see it as TERM.
pub const TERM: &str = "xterm-256color";

pub use bindings as ffi;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error(ffi::GhosttyResult);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ghostty error {}", self.0)
    }
}

impl std::error::Error for Error {}

trait GhosttyResultExt {
    fn into_result(self) -> Result<(), Error>;
}

impl GhosttyResultExt for ffi::GhosttyResult {
    fn into_result(self) -> Result<(), Error> {
        if self == ffi::GhosttyResult_GHOSTTY_SUCCESS {
            Ok(())
        } else {
            Err(Error(self))
        }
    }
}

const MODE_GRAPHEME_CLUSTER: u16 = 2027;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveScreen {
    Primary,
    Alternate,
}

type WritePtyCallback = dyn FnMut(&[u8]) + Send;

const MAX_CLIPBOARD_BYTES: usize = 192 * 1024;

#[derive(Default)]
struct TerminalCallbackState {
    write_pty: Option<Box<WritePtyCallback>>,
    size_report: ffi::GhosttySizeReportSize,
}

unsafe extern "C" fn size_trampoline(
    _terminal: ffi::GhosttyTerminal,
    userdata: *mut c_void,
    out_size: *mut ffi::GhosttySizeReportSize,
) -> bool {
    if userdata.is_null() || out_size.is_null() {
        return false;
    }
    let state = unsafe { &*userdata.cast::<TerminalCallbackState>() };
    let size = state.size_report;
    if size.rows == 0 || size.columns == 0 || size.cell_width == 0 || size.cell_height == 0 {
        return false;
    }
    unsafe {
        out_size.write(size);
    }
    true
}

unsafe extern "C" fn write_pty_trampoline(
    _terminal: ffi::GhosttyTerminal,
    userdata: *mut c_void,
    data: *const u8,
    len: usize,
) {
    if userdata.is_null() || (data.is_null() && len != 0) {
        return;
    }
    let state = unsafe { &mut *(userdata.cast::<TerminalCallbackState>()) };
    let Some(callback) = state.write_pty.as_mut() else {
        return;
    };
    let bytes = if len == 0 {
        &[]
    } else {
        unsafe { slice::from_raw_parts(data, len) }
    };
    callback(bytes);
}

unsafe extern "C" fn clipboard_write_trampoline(
    _terminal: ffi::GhosttyTerminal,
    userdata: *mut c_void,
    write: *const ffi::GhosttyClipboardWrite,
) {
    if write.is_null() || unsafe { (*write).size } < mem::size_of::<ffi::GhosttyClipboardWrite>() {
        return;
    }
    // The reply and request are borrowed only until this callback returns.
    let Some(reply) = (unsafe { (*write).reply }) else {
        return;
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: libghostty-vt owns these values for the synchronous callback.
        unsafe { clipboard_write_result(userdata, write) }
    }))
    .unwrap_or(ffi::GhosttyClipboardWriteResult_GHOSTTY_CLIPBOARD_WRITE_RESULT_INVALID_DATA);
    let response = ffi::GhosttyClipboardWriteReply {
        size: mem::size_of::<ffi::GhosttyClipboardWriteReply>(),
        result,
        remember: false,
    };
    unsafe { reply(write, &response) };
}

unsafe fn clipboard_write_result(
    userdata: *mut c_void,
    write: *const ffi::GhosttyClipboardWrite,
) -> ffi::GhosttyClipboardWriteResult {
    if userdata.is_null() || write.is_null() {
        return ffi::GhosttyClipboardWriteResult_GHOSTTY_CLIPBOARD_WRITE_RESULT_INVALID_DATA;
    }

    let required_size = std::mem::offset_of!(ffi::GhosttyClipboardWrite, contents_len)
        + std::mem::size_of::<usize>();
    // SAFETY: size is the leading field of the live request.
    if unsafe { (*write).size } < required_size {
        return ffi::GhosttyClipboardWriteResult_GHOSTTY_CLIPBOARD_WRITE_RESULT_INVALID_DATA;
    }
    // SAFETY: the size check covers every field accessed below.
    let request = unsafe { &*write };
    if request.location != ffi::GhosttyClipboardLocation_GHOSTTY_CLIPBOARD_LOCATION_STANDARD {
        return ffi::GhosttyClipboardWriteResult_GHOSTTY_CLIPBOARD_WRITE_RESULT_UNSUPPORTED;
    }

    if request.contents_len == 0 {
        return ffi::GhosttyClipboardWriteResult_GHOSTTY_CLIPBOARD_WRITE_RESULT_SUCCESS;
    }
    if request.contents_len != 1 {
        return ffi::GhosttyClipboardWriteResult_GHOSTTY_CLIPBOARD_WRITE_RESULT_UNSUPPORTED;
    }
    if request.contents.is_null() {
        return ffi::GhosttyClipboardWriteResult_GHOSTTY_CLIPBOARD_WRITE_RESULT_INVALID_DATA;
    }

    // SAFETY: libghostty-vt keeps the single content and its strings alive for the callback.
    let content = unsafe { &*request.contents };
    // SAFETY: the MIME string is borrowed from the live callback request.
    let Some(mime) = (unsafe { borrowed_bytes(content.mime) }) else {
        return ffi::GhosttyClipboardWriteResult_GHOSTTY_CLIPBOARD_WRITE_RESULT_INVALID_DATA;
    };
    let is_text = std::str::from_utf8(mime)
        .ok()
        .and_then(|mime| mime.split(';').next())
        .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("text/plain"));
    if !is_text {
        return ffi::GhosttyClipboardWriteResult_GHOSTTY_CLIPBOARD_WRITE_RESULT_UNSUPPORTED;
    }

    // SAFETY: the data string is borrowed from the live callback request.
    let Some(bytes) = (unsafe { borrowed_bytes(content.data) }) else {
        return ffi::GhosttyClipboardWriteResult_GHOSTTY_CLIPBOARD_WRITE_RESULT_INVALID_DATA;
    };
    if bytes.is_empty() {
        return ffi::GhosttyClipboardWriteResult_GHOSTTY_CLIPBOARD_WRITE_RESULT_UNSUPPORTED;
    }
    if bytes.len() > MAX_CLIPBOARD_BYTES {
        return ffi::GhosttyClipboardWriteResult_GHOSTTY_CLIPBOARD_WRITE_RESULT_INVALID_DATA;
    }
    // Match Herdr's protocol reply. The kernel has no clipboard consumer.
    ffi::GhosttyClipboardWriteResult_GHOSTTY_CLIPBOARD_WRITE_RESULT_SUCCESS
}

unsafe fn borrowed_bytes<'a>(value: ffi::GhosttyString) -> Option<&'a [u8]> {
    if value.len == 0 {
        Some(&[])
    } else if value.ptr.is_null() {
        None
    } else {
        // SAFETY: the callback contract keeps pointer and length valid until return.
        Some(unsafe { slice::from_raw_parts(value.ptr, value.len) })
    }
}

pub struct Terminal {
    raw: ffi::GhosttyTerminal,
    callback_state: Box<TerminalCallbackState>,
}

impl Terminal {
    pub fn new(cols: u16, rows: u16, max_scrollback: usize) -> Result<Self, Error> {
        let mut raw = ptr::null_mut();
        // SAFETY: valid out pointer, null allocator means default allocator.
        unsafe {
            ffi::ghostty_terminal_new(ptr::null(), &mut raw, cols, rows).into_result()?;
        }

        let mut terminal = Self {
            raw,
            callback_state: Box::new(TerminalCallbackState {
                size_report: ffi::GhosttySizeReportSize {
                    rows,
                    columns: cols,
                    ..Default::default()
                },
                ..Default::default()
            }),
        };
        let userdata = (&mut *terminal.callback_state as *mut TerminalCallbackState).cast();
        let glyph_protocol = false;
        // libghostty enables image storage by default; Herdr opts in explicitly.
        let kitty_storage_limit: u64 = 0;
        let terminfo_name = ffi::GhosttyString {
            ptr: TERM.as_ptr().cast(),
            len: TERM.len(),
        };
        let grapheme_default = ffi::GhosttyTerminalModeConfig {
            mode: MODE_GRAPHEME_CLUSTER,
            value: true,
        };
        let clipboard_callback: ffi::GhosttyTerminalClipboardWriteFn =
            Some(clipboard_write_trampoline);
        unsafe {
            ffi::ghostty_terminal_set(
                terminal.raw,
                ffi::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_STORAGE_LIMIT,
                (&kitty_storage_limit as *const u64).cast(),
            )
            .into_result()?;
            ffi::ghostty_terminal_set(
                terminal.raw,
                ffi::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_TERMINFO_NAME,
                (&terminfo_name as *const ffi::GhosttyString).cast(),
            )
            .into_result()?;
            ffi::ghostty_terminal_set(
                terminal.raw,
                ffi::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES,
                (&max_scrollback as *const usize).cast(),
            )
            .into_result()?;
            ffi::ghostty_terminal_set(
                terminal.raw,
                ffi::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_MODE_DEFAULT,
                (&grapheme_default as *const ffi::GhosttyTerminalModeConfig).cast(),
            )
            .into_result()?;
            ffi::ghostty_terminal_set(
                terminal.raw,
                ffi::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_USERDATA,
                userdata,
            )
            .into_result()?;
            ffi::ghostty_terminal_set(
                terminal.raw,
                ffi::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_SIZE,
                (size_trampoline as *const ()).cast(),
            )
            .into_result()?;
            ffi::ghostty_terminal_set(
                terminal.raw,
                ffi::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_CLIPBOARD_WRITE,
                clipboard_callback.map_or(ptr::null(), |callback| (callback as *const ()).cast()),
            )
            .into_result()?;
            ffi::ghostty_terminal_set(
                terminal.raw,
                ffi::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_GLYPH_PROTOCOL,
                (&glyph_protocol as *const bool).cast(),
            )
            .into_result()?;
        }
        Ok(terminal)
    }

    pub fn write(&mut self, bytes: &[u8]) {
        // SAFETY: self.raw is a live terminal handle for self's lifetime.
        unsafe {
            ffi::ghostty_terminal_vt_write(self.raw, bytes.as_ptr(), bytes.len());
        }
    }

    pub fn resize(
        &mut self,
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    ) -> Result<(), Error> {
        let size_report = ffi::GhosttySizeReportSize {
            rows,
            columns: cols,
            cell_width: cell_width_px,
            cell_height: cell_height_px,
        };
        // SAFETY: self.raw is valid and sizes are plain values.
        unsafe {
            ffi::ghostty_terminal_resize(
                self.raw,
                cols,
                rows,
                cell_width_px.max(1),
                cell_height_px.max(1),
            )
            .into_result()?;
        }
        self.callback_state.size_report = size_report;
        Ok(())
    }

    pub fn set_write_pty_callback<F>(&mut self, callback: F) -> Result<(), Error>
    where
        F: FnMut(&[u8]) + Send + 'static,
    {
        unsafe {
            ffi::ghostty_terminal_set(
                self.raw,
                ffi::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_WRITE_PTY,
                (write_pty_trampoline as *const ()).cast(),
            )
            .into_result()?;
        }
        self.callback_state.write_pty = Some(Box::new(callback));
        Ok(())
    }

    pub fn active_screen(&self) -> Result<ActiveScreen, Error> {
        let mut out = ffi::GhosttyTerminalScreen_GHOSTTY_TERMINAL_SCREEN_PRIMARY;
        unsafe {
            ffi::ghostty_terminal_get(
                self.raw,
                ffi::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN,
                (&mut out as *mut ffi::GhosttyTerminalScreen).cast(),
            )
            .into_result()?;
        }
        Ok(match out {
            ffi::GhosttyTerminalScreen_GHOSTTY_TERMINAL_SCREEN_PRIMARY => ActiveScreen::Primary,
            ffi::GhosttyTerminalScreen_GHOSTTY_TERMINAL_SCREEN_ALTERNATE => ActiveScreen::Alternate,
            _ => ActiveScreen::Primary,
        })
    }

    pub fn read_text_viewport(
        &self,
        start: (u16, u32),
        end: (u16, u32),
        rectangle: bool,
    ) -> Result<String, Error> {
        self.format_viewport(
            start,
            end,
            rectangle,
            ffi::GhosttyFormatterFormat_GHOSTTY_FORMATTER_FORMAT_PLAIN,
        )
    }

    pub fn read_styled_viewport(&self, end: (u16, u32)) -> Result<String, Error> {
        self.format_viewport(
            (0, 0),
            end,
            false,
            ffi::GhosttyFormatterFormat_GHOSTTY_FORMATTER_FORMAT_VT,
        )
    }

    fn format_viewport(
        &self,
        start: (u16, u32),
        end: (u16, u32),
        rectangle: bool,
        emit: ffi::GhosttyFormatterFormat,
    ) -> Result<String, Error> {
        let start = ghostty_viewport_point(start.0, start.1);
        let end = ghostty_viewport_point(end.0, end.1);
        let mut start_ref = ffi::GhosttyGridRef {
            size: mem::size_of::<ffi::GhosttyGridRef>(),
            ..Default::default()
        };
        let mut end_ref = ffi::GhosttyGridRef {
            size: mem::size_of::<ffi::GhosttyGridRef>(),
            ..Default::default()
        };
        unsafe {
            ffi::ghostty_terminal_grid_ref(self.raw, start, &mut start_ref).into_result()?;
            ffi::ghostty_terminal_grid_ref(self.raw, end, &mut end_ref).into_result()?;
        }

        let selection = ffi::GhosttySelection {
            size: mem::size_of::<ffi::GhosttySelection>(),
            start: start_ref,
            end: end_ref,
            rectangle,
        };
        let mut formatter: ffi::GhosttyFormatter = ptr::null_mut();
        let options = ffi::GhosttyFormatterTerminalOptions {
            size: mem::size_of::<ffi::GhosttyFormatterTerminalOptions>(),
            emit,
            unwrap: true,
            trim: true,
            extra: ffi::GhosttyFormatterTerminalExtra {
                size: mem::size_of::<ffi::GhosttyFormatterTerminalExtra>(),
                screen: ffi::GhosttyFormatterScreenExtra {
                    size: mem::size_of::<ffi::GhosttyFormatterScreenExtra>(),
                    ..Default::default()
                },
                ..Default::default()
            },
            selection: &selection,
        };
        unsafe {
            ffi::ghostty_formatter_terminal_new(ptr::null(), &mut formatter, self.raw, options)
                .into_result()?;
        }

        let mut out_ptr = ptr::null_mut();
        let mut out_len = 0usize;
        let result = unsafe {
            ffi::ghostty_formatter_format_alloc(formatter, ptr::null(), &mut out_ptr, &mut out_len)
        };
        unsafe {
            ffi::ghostty_formatter_free(formatter);
        }
        result.into_result()?;

        let text = if out_len == 0 {
            String::new()
        } else {
            let bytes = unsafe { slice::from_raw_parts(out_ptr.cast_const(), out_len) };
            String::from_utf8_lossy(bytes).into_owned()
        };

        if !out_ptr.is_null() {
            unsafe {
                ffi::ghostty_free(ptr::null(), out_ptr, out_len);
            }
        }

        Ok(text)
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        // SAFETY: freeing a null or live handle is allowed by the C API.
        unsafe {
            ffi::ghostty_terminal_free(self.raw);
        }
    }
}

fn ghostty_viewport_point(x: u16, y: u32) -> ffi::GhosttyPoint {
    ffi::GhosttyPoint {
        tag: ffi::GhosttyPointTag_GHOSTTY_POINT_TAG_VIEWPORT,
        value: ffi::GhosttyPointValue {
            coordinate: ffi::GhosttyPointCoordinate { x, y },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn formats_the_active_viewport_and_restores_the_primary_screen() {
        let mut term = Terminal::new(20, 4, 1000).unwrap();
        term.write("old\r\x1b[2K主屏".as_bytes());
        term.write(b"\x1b[?1049h\x1b[Halt");
        let view = |term: &Terminal| term.read_text_viewport((0, 0), (19, 3), false).unwrap();
        assert_eq!(term.active_screen().unwrap(), ActiveScreen::Alternate);
        assert_eq!(view(&term).trim(), "alt");
        term.write(b"\x1b[?1049l");
        assert_eq!(term.active_screen().unwrap(), ActiveScreen::Primary);
        assert_eq!(view(&term).trim(), "主屏");
    }

    #[test]
    fn sends_query_replies_after_resizing() {
        let mut term = Terminal::new(20, 4, 1000).unwrap();
        let replies = Arc::new(Mutex::new(Vec::new()));
        let output = Arc::clone(&replies);
        term.set_write_pty_callback(move |bytes| output.lock().unwrap().extend_from_slice(bytes))
            .unwrap();
        term.resize(37, 11, 1, 1).unwrap();
        term.write(b"\x1b[11;37H\x1b[6n");
        assert_eq!(&*replies.lock().unwrap(), b"\x1b[11;37R");
    }
}

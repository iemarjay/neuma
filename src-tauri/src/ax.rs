/// macOS Accessibility API helpers.
///
/// Public entry point:
///   - `read_context(max_chars)` — read up to `max_chars` characters before the
///     cursor from the focused field, for context-aware cleanup.
///
/// AX text injection (`AXSelectedText`) is stubbed — re-enable in typer.rs once
/// we can guarantee the target app (not the Neuma overlay) holds focus at inject time.

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::{CStr, CString};
    use std::os::raw::{c_char, c_void};

    // kCFStringEncodingUTF8
    const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
    // kAXValueCFRangeType = 4
    const AX_VALUE_CF_RANGE_TYPE: u32 = 4;
    // kAXErrorSuccess
    const AX_ERROR_SUCCESS: i32 = 0;

    /// Mirror of CFRange: { location: CFIndex, length: CFIndex }.
    /// CFIndex = signed long = isize on all Apple 64-bit platforms.
    #[repr(C)]
    struct CFRange {
        location: isize,
        length: isize,
    }

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXUIElementCreateSystemWide() -> *mut c_void;
        fn AXUIElementCopyAttributeValue(
            element: *mut c_void,
            attribute: *const c_void, // CFStringRef
            value: *mut *mut c_void,  // CFTypeRef *
        ) -> i32;
        fn AXValueGetValue(
            value: *mut c_void,
            ax_type: u32,
            result: *mut c_void,
        ) -> bool;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFStringCreateWithCString(
            alloc: *const c_void,
            c_str: *const c_char,
            encoding: u32,
        ) -> *mut c_void;
        fn CFStringGetLength(string: *mut c_void) -> isize;
        fn CFStringGetCString(
            string: *mut c_void,
            buffer: *mut c_char,
            buffer_size: isize,
            encoding: u32,
        ) -> bool;
        fn CFRelease(cf: *const c_void);
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Create a CFString from a Rust str. Caller must CFRelease.
    fn make_cfstring(s: &str) -> *mut c_void {
        let c = CString::new(s).unwrap_or_default();
        unsafe { CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), CF_STRING_ENCODING_UTF8) }
    }

    /// Convert a CFString to a Rust String. Does NOT release the CFString.
    fn cfstring_to_string(cfstr: *mut c_void) -> Option<String> {
        if cfstr.is_null() {
            return None;
        }
        unsafe {
            let char_len = CFStringGetLength(cfstr);
            // UTF-8 worst case: 4 bytes per UTF-16 unit + null terminator.
            let buf_size = char_len * 4 + 1;
            let mut buf: Vec<u8> = vec![0u8; buf_size as usize];
            let ok = CFStringGetCString(
                cfstr,
                buf.as_mut_ptr() as *mut c_char,
                buf_size,
                CF_STRING_ENCODING_UTF8,
            );
            if !ok {
                return None;
            }
            let c_str = CStr::from_ptr(buf.as_ptr() as *const c_char);
            Some(c_str.to_string_lossy().into_owned())
        }
    }

    /// Returns the currently focused AXUIElement. Caller must CFRelease.
    fn focused_element() -> Option<*mut c_void> {
        unsafe {
            let system = AXUIElementCreateSystemWide();
            if system.is_null() {
                return None;
            }
            let attr = make_cfstring("AXFocusedUIElement");
            let mut focused: *mut c_void = std::ptr::null_mut();
            let err = AXUIElementCopyAttributeValue(system, attr, &mut focused);
            CFRelease(attr);
            CFRelease(system);
            if err != AX_ERROR_SUCCESS || focused.is_null() {
                return None;
            }
            Some(focused)
        }
    }

    // ── Public API ────────────────────────────────────────────────────────────

    /// Read up to `max_chars` characters immediately before the cursor from the
    /// focused element's text value. Returns `None` if the element does not
    /// expose readable text (e.g. Electron apps, terminals, browsers).
    pub fn read_context(max_chars: usize) -> Option<String> {
        let focused = focused_element()?;

        unsafe {
            // ── Read full field text ──────────────────────────────────────────
            let attr_value = make_cfstring("AXValue");
            let mut cf_value: *mut c_void = std::ptr::null_mut();
            let err = AXUIElementCopyAttributeValue(focused, attr_value, &mut cf_value);
            CFRelease(attr_value);

            if err != AX_ERROR_SUCCESS || cf_value.is_null() {
                CFRelease(focused);
                return None;
            }

            let full_text = cfstring_to_string(cf_value);
            CFRelease(cf_value);

            let text = full_text?;

            // ── Read cursor position ──────────────────────────────────────────
            let attr_range = make_cfstring("AXSelectedTextRange");
            let mut cf_range_ref: *mut c_void = std::ptr::null_mut();
            let err2 =
                AXUIElementCopyAttributeValue(focused, attr_range, &mut cf_range_ref);
            CFRelease(attr_range);
            CFRelease(focused);

            let chars: Vec<char> = text.chars().collect();

            if err2 != AX_ERROR_SUCCESS || cf_range_ref.is_null() {
                // No cursor info — return the last max_chars of the field.
                let start = chars.len().saturating_sub(max_chars);
                return Some(chars[start..].iter().collect());
            }

            let mut range = CFRange { location: 0, length: 0 };
            AXValueGetValue(
                cf_range_ref,
                AX_VALUE_CF_RANGE_TYPE,
                &mut range as *mut CFRange as *mut c_void,
            );
            CFRelease(cf_range_ref);

            // Extract text before the cursor.
            let cursor = (range.location as usize).min(chars.len());
            let start = cursor.saturating_sub(max_chars);
            Some(chars[start..cursor].iter().collect())
        }
    }
}

#[cfg(target_os = "macos")]
pub use imp::read_context;

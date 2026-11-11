//! Strict Unicode codecs and a small whitelist of bounded native iconv codecs.
use anyhow::{Context, Result, bail, ensure};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Order {
    Little,
    Big,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Codec {
    Utf8,
    Utf16,
    Utf16Le,
    Utf16Be,
    Utf16LeBom,
    Utf32,
    Utf32Le,
    Utf32Be,
    Legacy(String),
}

// The BOM belongs to the file, not its first line. Fragment codecs never
// emit one and follow the input byte order, even if Git would choose a
// different order when checking out the whole file.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(super) struct LineLayout<'a> {
    pub prefix: &'a [u8],
    pub body: &'a [u8],
    pub delimiter: &'static [u8],
    pub fragments: Codec,
}

impl Codec {
    pub(super) fn parse(label: &[u8]) -> Result<Self> {
        let label = std::str::from_utf8(label)
            .context("working-tree-encoding must have a UTF-8 name")?
            .to_ascii_uppercase();
        Ok(match label.as_str() {
            "UTF-8" | "UTF8" => Self::Utf8,
            "UTF-16" => Self::Utf16,
            "UTF-16LE" => Self::Utf16Le,
            "UTF-16BE" => Self::Utf16Be,
            "UTF-16LE-BOM" => Self::Utf16LeBom,
            "UTF-32" => Self::Utf32,
            "UTF-32LE" => Self::Utf32Le,
            "UTF-32BE" => Self::Utf32Be,
            "SHIFT-JIS" | "SHIFT_JIS" | "WINDOWS-1252" | "CP1252" | "ISO-8859-1" | "ASCII"
            | "US-ASCII" => Self::Legacy(label),
            _ => bail!("Unsupported working-tree-encoding: {label}"),
        })
    }
    pub(super) fn decode(&self, bytes: &[u8], limit: usize) -> Result<Vec<u8>> {
        ensure!(
            bytes.len() <= limit,
            "Encoded working file exceeds the byte limit"
        );
        if bytes.is_empty() {
            return Ok(Vec::new());
        }
        let output = match self {
            Self::Utf8 => {
                std::str::from_utf8(bytes).context("Invalid UTF-8 working file")?;
                bytes.to_vec()
            }
            Self::Utf16 | Self::Utf16Le | Self::Utf16Be | Self::Utf16LeBom => {
                let bom = bom16(bytes);
                let (order, body) = match self {
                    Self::Utf16 => (bom.context("UTF-16 requires a BOM")?, &bytes[2..]),
                    Self::Utf16LeBom => match bom {
                        Some(order) => (order, &bytes[2..]),
                        // Git treats the input side of its -LE-BOM alias as UTF-16.
                        None => (native_default_input16()?, bytes),
                    },
                    Self::Utf16Le => {
                        ensure!(bom.is_none(), "UTF-16LE prohibits a BOM");
                        (Order::Little, bytes)
                    }
                    Self::Utf16Be => {
                        ensure!(bom.is_none(), "UTF-16BE prohibits a BOM");
                        (Order::Big, bytes)
                    }
                    _ => unreachable!(),
                };
                decode16(body, order, limit)?
            }
            Self::Utf32 | Self::Utf32Le | Self::Utf32Be => {
                let bom = bom32(bytes);
                let (order, body) = match self {
                    Self::Utf32 => (bom.context("UTF-32 requires a BOM")?, &bytes[4..]),
                    Self::Utf32Le => {
                        ensure!(bom.is_none(), "UTF-32LE prohibits a BOM");
                        (Order::Little, bytes)
                    }
                    Self::Utf32Be => {
                        ensure!(bom.is_none(), "UTF-32BE prohibits a BOM");
                        (Order::Big, bytes)
                    }
                    _ => unreachable!(),
                };
                decode32(body, order, limit)?
            }
            Self::Legacy(label) => native::convert(label, "UTF-8", bytes, limit)?,
        };
        std::str::from_utf8(&output).context("Encoding did not produce valid UTF-8")?;
        Ok(output)
    }
    pub(super) fn encode(&self, bytes: &[u8], limit: usize) -> Result<Vec<u8>> {
        ensure!(
            bytes.len() <= limit,
            "Canonical encoded file exceeds the byte limit"
        );
        let text = std::str::from_utf8(bytes)
            .context("Canonical working-tree-encoding content is not UTF-8")?;
        if bytes.is_empty() {
            return Ok(Vec::new());
        }
        match self {
            Self::Utf8 => Ok(bytes.to_vec()),
            Self::Utf16 => encode16(text, native_default_output("UTF-16", 2)?, true, limit),
            Self::Utf16Le => encode16(text, Order::Little, false, limit),
            Self::Utf16Be => encode16(text, Order::Big, false, limit),
            Self::Utf16LeBom => encode16(text, Order::Little, true, limit),
            Self::Utf32 => encode32(text, native_default_output("UTF-32", 4)?, true, limit),
            Self::Utf32Le => encode32(text, Order::Little, false, limit),
            Self::Utf32Be => encode32(text, Order::Big, false, limit),
            Self::Legacy(label) => native::convert("UTF-8", label, bytes, limit),
        }
    }

    // Only called after a strict decode of the full source has succeeded.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub(super) fn line_layout<'a>(&self, raw: &'a [u8]) -> Result<LineLayout<'a>> {
        let utf16 = |order, bom: &'static [u8]| {
            let (delimiter, fragments): (&'static [u8], _) = match order {
                Order::Little => (b"\n\0", Self::Utf16Le),
                Order::Big => (b"\0\n", Self::Utf16Be),
            };
            LineLayout {
                prefix: bom,
                body: if raw.starts_with(bom) {
                    &raw[bom.len()..]
                } else {
                    raw
                },
                delimiter,
                fragments,
            }
        };
        let utf32 = |order, bom: &'static [u8]| {
            let (delimiter, fragments): (&'static [u8], _) = match order {
                Order::Little => (b"\n\0\0\0", Self::Utf32Le),
                Order::Big => (b"\0\0\0\n", Self::Utf32Be),
            };
            LineLayout {
                prefix: bom,
                body: if raw.starts_with(bom) {
                    &raw[bom.len()..]
                } else {
                    raw
                },
                delimiter,
                fragments,
            }
        };
        Ok(match self {
            Self::Utf16Le => utf16(Order::Little, b""),
            Self::Utf16Be => utf16(Order::Big, b""),
            Self::Utf16 | Self::Utf16LeBom => match bom16(raw) {
                Some(Order::Little) => utf16(Order::Little, b"\xff\xfe"),
                Some(Order::Big) => utf16(Order::Big, b"\xfe\xff"),
                None if raw.is_empty() => {
                    if *self == Self::Utf16LeBom
                        || native_default_output("UTF-16", 2)? == Order::Little
                    {
                        utf16(Order::Little, b"\xff\xfe")
                    } else {
                        utf16(Order::Big, b"\xfe\xff")
                    }
                }
                None => utf16(native_default_input16()?, b""),
            },
            Self::Utf32Le => utf32(Order::Little, b""),
            Self::Utf32Be => utf32(Order::Big, b""),
            Self::Utf32 => {
                let order = match bom32(raw) {
                    Some(order) => order,
                    None => native_default_output("UTF-32", 4)?,
                };
                match order {
                    Order::Little => utf32(Order::Little, b"\xff\xfe\0\0"),
                    Order::Big => utf32(Order::Big, b"\0\0\xfe\xff"),
                }
            }
            // UTF-8 and the whitelisted stateless legacy codecs never contain
            // an ASCII LF in a multibyte code unit.
            Self::Utf8 | Self::Legacy(_) => LineLayout {
                prefix: b"",
                body: raw,
                delimiter: b"\n",
                fragments: self.clone(),
            },
        })
    }
}
fn bom16(bytes: &[u8]) -> Option<Order> {
    if bytes.starts_with(&[0xff, 0xfe]) {
        Some(Order::Little)
    } else if bytes.starts_with(&[0xfe, 0xff]) {
        Some(Order::Big)
    } else {
        None
    }
}
fn bom32(bytes: &[u8]) -> Option<Order> {
    if bytes.starts_with(&[0xff, 0xfe, 0, 0]) {
        Some(Order::Little)
    } else if bytes.starts_with(&[0, 0, 0xfe, 0xff]) {
        Some(Order::Big)
    } else {
        None
    }
}
fn unit16(bytes: &[u8], order: Order) -> u16 {
    let bytes = [bytes[0], bytes[1]];
    match order {
        Order::Little => u16::from_le_bytes(bytes),
        Order::Big => u16::from_be_bytes(bytes),
    }
}
fn unit32(bytes: &[u8], order: Order) -> u32 {
    let bytes = [bytes[0], bytes[1], bytes[2], bytes[3]];
    match order {
        Order::Little => u32::from_le_bytes(bytes),
        Order::Big => u32::from_be_bytes(bytes),
    }
}
fn decode16(bytes: &[u8], order: Order, limit: usize) -> Result<Vec<u8>> {
    ensure!(bytes.len().is_multiple_of(2), "Invalid UTF-16 byte length");
    let characters = || {
        std::char::decode_utf16(
            bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|chunk| unit16(chunk, order)),
        )
    };
    let mut size = 0usize;
    for character in characters() {
        let character = character.context("Invalid UTF-16 surrogate sequence")?;
        size = size
            .checked_add(character.len_utf8())
            .context("Encoding size overflow")?;
        ensure!(
            size <= limit,
            "Decoded canonical content exceeds the byte limit"
        );
    }
    let mut output = String::with_capacity(size);
    for character in characters() {
        output.push(character.context("Invalid UTF-16 surrogate sequence")?);
    }
    Ok(output.into_bytes())
}
fn decode32(bytes: &[u8], order: Order, limit: usize) -> Result<Vec<u8>> {
    ensure!(bytes.len().is_multiple_of(4), "Invalid UTF-32 byte length");
    let mut size = 0usize;
    for chunk in bytes.as_chunks::<4>().0 {
        let character = char::from_u32(unit32(chunk, order)).context("Invalid UTF-32 scalar")?;
        size = size
            .checked_add(character.len_utf8())
            .context("Encoding size overflow")?;
        ensure!(
            size <= limit,
            "Decoded canonical content exceeds the byte limit"
        );
    }
    let mut output = String::with_capacity(size);
    for chunk in bytes.as_chunks::<4>().0 {
        output.push(char::from_u32(unit32(chunk, order)).context("Invalid UTF-32 scalar")?);
    }
    Ok(output.into_bytes())
}
fn encode16(text: &str, order: Order, bom: bool, limit: usize) -> Result<Vec<u8>> {
    let size = text
        .encode_utf16()
        .count()
        .checked_mul(2)
        .and_then(|size| size.checked_add(if bom { 2 } else { 0 }))
        .context("Encoding size overflow")?;
    ensure!(size <= limit, "Encoded working file exceeds the byte limit");
    let mut output = Vec::with_capacity(size);
    if bom {
        output.extend_from_slice(match order {
            Order::Little => &[0xff, 0xfe],
            Order::Big => &[0xfe, 0xff],
        });
    }
    for unit in text.encode_utf16() {
        output.extend_from_slice(&match order {
            Order::Little => unit.to_le_bytes(),
            Order::Big => unit.to_be_bytes(),
        });
    }
    Ok(output)
}
fn encode32(text: &str, order: Order, bom: bool, limit: usize) -> Result<Vec<u8>> {
    let size = text
        .chars()
        .count()
        .checked_mul(4)
        .and_then(|size| size.checked_add(if bom { 4 } else { 0 }))
        .context("Encoding size overflow")?;
    ensure!(size <= limit, "Encoded working file exceeds the byte limit");
    let mut output = Vec::with_capacity(size);
    if bom {
        output.extend_from_slice(match order {
            Order::Little => &[0xff, 0xfe, 0, 0],
            Order::Big => &[0, 0, 0xfe, 0xff],
        });
    }
    for character in text.chars() {
        let unit = character as u32;
        output.extend_from_slice(&match order {
            Order::Little => unit.to_le_bytes(),
            Order::Big => unit.to_be_bytes(),
        });
    }
    Ok(output)
}
fn native_default_output(label: &str, width: usize) -> Result<Order> {
    let output = native::convert("UTF-8", label, b"A", 16)?;
    let order = if width == 2 {
        bom16(&output)
    } else {
        bom32(&output)
    }
    .context("Native generic Unicode encoding did not supply a BOM")?;
    let expected = if width == 2 {
        encode16("A", order, true, 16)?
    } else {
        encode32("A", order, true, 16)?
    };
    ensure!(
        output == expected,
        "Unsupported native generic Unicode byte order"
    );
    Ok(order)
}
fn native_default_input16() -> Result<Order> {
    let output = native::convert("UTF-16", "UTF-8", &[0, b'A'], 8)?;
    match output.as_slice() {
        b"A" => Ok(Order::Big),
        [0xe4, 0x84, 0x80] => Ok(Order::Little),
        _ => bail!("Unsupported native UTF-16 input byte order"),
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod native {
    use anyhow::{Context, Result, ensure};
    use std::ffi::{CString, c_char, c_int, c_void};
    #[cfg_attr(target_os = "macos", link(name = "iconv"))]
    unsafe extern "C" {
        fn iconv_open(to: *const c_char, from: *const c_char) -> *mut c_void;
        fn iconv(
            descriptor: *mut c_void,
            input: *mut *mut c_char,
            input_left: *mut usize,
            output: *mut *mut c_char,
            output_left: *mut usize,
        ) -> usize;
        fn iconv_close(descriptor: *mut c_void) -> c_int;
    }
    struct Descriptor(*mut c_void);
    impl Drop for Descriptor {
        fn drop(&mut self) {
            // SAFETY: a successful iconv_open is owned by this descriptor exactly once.
            unsafe {
                iconv_close(self.0);
            }
        }
    }
    pub(super) fn convert(from: &str, to: &str, input: &[u8], limit: usize) -> Result<Vec<u8>> {
        ensure!(limit > 0, "Invalid encoding byte limit");
        let from_name = CString::new(from)?;
        let to_name = CString::new(to)?;
        // SAFETY: both converter names are NUL-terminated; input names are whitelisted.
        let descriptor = unsafe { iconv_open(to_name.as_ptr(), from_name.as_ptr()) };
        ensure!(
            !descriptor.is_null() && descriptor != usize::MAX as *mut c_void,
            "Native working-tree-encoding is unavailable: {from} to {to}"
        );
        let descriptor = Descriptor(descriptor);
        let mut output = vec![0u8; input.len().saturating_add(16).max(16).min(limit)];
        let mut input_pointer = input.as_ptr().cast_mut().cast::<c_char>();
        let mut input_left = input.len();
        let mut written = 0usize;
        for flush in [false, true] {
            loop {
                let available = output.len() - written;
                let mut output_left = available;
                // SAFETY: written <= initialized output length, and iconv is
                // offered only the remaining valid bytes. POSIX iconv reads its
                // input despite its historical mutable char** signature.
                let mut output_pointer =
                    unsafe { output.as_mut_ptr().add(written) }.cast::<c_char>();
                let result = unsafe {
                    iconv(
                        descriptor.0,
                        if flush {
                            std::ptr::null_mut()
                        } else {
                            &mut input_pointer
                        },
                        if flush {
                            std::ptr::null_mut()
                        } else {
                            &mut input_left
                        },
                        &mut output_pointer,
                        &mut output_left,
                    )
                };
                ensure!(
                    output_left <= available,
                    "Invalid native encoding output length"
                );
                written += available - output_left;
                if result != usize::MAX {
                    ensure!(
                        result == 0,
                        "Native encoding would lose characters: {from} to {to}"
                    );
                    ensure!(
                        flush || input_left == 0,
                        "Native encoding did not consume its input"
                    );
                    break;
                }
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::E2BIG) {
                    return Err(error).with_context(|| format!("Invalid or unrepresentable working-tree-encoding content: {from} to {to}"));
                }
                ensure!(
                    output.len() < limit,
                    "Encoded or decoded content exceeds the byte limit"
                );
                let next = output.len().checked_mul(2).unwrap_or(limit).min(limit);
                output.resize(next, 0);
            }
        }
        output.truncate(written);
        Ok(output)
    }
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod native {
    use anyhow::{Result, bail};
    pub(super) fn convert(from: &str, to: &str, _: &[u8], _: usize) -> Result<Vec<u8>> {
        bail!(
            "Native legacy/generic working-tree-encoding is not supported on this platform: {from} to {to}; explicit UTF-8/UTF-16LE/BE/UTF-32LE/BE remain available"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::Codec;
    #[test]
    fn empty_unicode_files_do_not_acquire_a_bom() {
        for label in [
            "UTF-8",
            "UTF-16",
            "UTF-16LE",
            "UTF-16BE",
            "UTF-16LE-BOM",
            "UTF-32",
            "UTF-32LE",
            "UTF-32BE",
        ] {
            let codec = Codec::parse(label.as_bytes()).unwrap();
            assert!(codec.decode(b"", 16).unwrap().is_empty());
            assert!(codec.encode(b"", 16).unwrap().is_empty());
        }
    }
    #[test]
    fn malformed_scalars_and_oversized_unicode_output_fail_closed() {
        assert!(Codec::Utf16Le.decode(&[0, 0xd8], 16).is_err());
        assert!(Codec::Utf16Le.decode(&[0], 16).is_err());
        assert!(
            Codec::Utf32Le
                .decode(&0x11_0000u32.to_le_bytes(), 16)
                .is_err()
        );
        assert!(Codec::Utf32Be.decode(&0xd800u32.to_be_bytes(), 16).is_err());
        assert!(Codec::Utf32Le.encode(b"abcde", 16).is_err());
        assert!(Codec::Utf16Le.decode(&[0x65, 0x65, 0x65, 0x65], 4).is_err());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn native_legacy_conversion_is_lossless_and_accepts_exact_byte_limits() {
        let ascii = Codec::parse(b"ASCII").unwrap();
        assert!(ascii.encode("😀".as_bytes(), 16).is_err());
        assert!(ascii.decode(&[0xff], 16).is_err());
        assert_eq!(ascii.encode(b"1234", 4).unwrap(), b"1234");
        let latin = Codec::parse(b"ISO-8859-1").unwrap();
        assert_eq!(latin.decode(&[0xff], 2).unwrap(), "ÿ".as_bytes());
        assert!(latin.decode(&[0xff], 1).is_err());
        assert!(Codec::parse(b"UTF-8//IGNORE").is_err());
    }
}

//! Bootconfig block at the end of the initrd, and kernel command line.
//!
//! Format (the kernel's `Documentation/admin-guide/bootconfig.rst` and
//! `tools/bootconfig/main.c`):
//!
//! ```text
//! [initrd][bootconfig][\0 and padding][size (le32)][checksum (le32)][#BOOTCONFIG\n]
//! ```
//!
//! The text ends with a NUL (like `tools/bootconfig`, which counts
//! `strlen + 1`); the NUL padding brings the total length of the initrd
//! to a multiple of 4; `size` counts text, NUL and padding; `checksum` is the
//! 32-bit sum of the bytes of that area. The kernel (`init/main.c`,
//! `get_boot_config_from_initrd`) looks for the magic at the end of
//! `linux,initrd-end`, checks the checksum and shortens the initrd before
//! opening it; the block is used only if the command line contains `bootconfig`
//! (or with `CONFIG_BOOT_CONFIG_FORCE`).

/// Final magic.
pub const MAGIC: &[u8; 12] = b"#BOOTCONFIG\n";
/// The kernel's `XBC_DATA_MAX`: maximum size of text and padding.
pub const MAX_SIZE: usize = 32767;

/// 32-bit sum of the bytes (`xbc_calc_checksum`).
pub fn checksum(data: &[u8]) -> u32 {
    data.iter().fold(0u32, |s, &b| s.wrapping_add(b as u32))
}

/// Appends to `initrd` the block with the text `params` (one line per
/// parameter). Error if the block exceeds [`MAX_SIZE`].
pub fn append(initrd: &mut Vec<u8>, params: &[u8]) -> Result<(), String> {
    let mut data = params.to_vec();
    data.push(0);
    let total = initrd.len() + data.len() + 8 + MAGIC.len();
    data.resize(data.len() + total.next_multiple_of(4) - total, 0);
    if data.len() > MAX_SIZE {
        return Err(format!("bootconfig of {} bytes, over the maximum of {MAX_SIZE}", data.len()));
    }
    let csum = checksum(&data);
    initrd.extend_from_slice(&data);
    initrd.extend_from_slice(&(data.len() as u32).to_le_bytes());
    initrd.extend_from_slice(&csum.to_le_bytes());
    initrd.extend_from_slice(MAGIC);
    Ok(())
}

/// The inverse of [`append`], done the way the kernel does it: if `initrd` ends
/// with a valid block (magic up to 3 bytes before the end, correct
/// checksum) returns the length of the real initrd and the text without the trailing
/// NULs.
pub fn split(initrd: &[u8]) -> Option<(usize, &[u8])> {
    let end = (0..4).find_map(|i| {
        let e = initrd.len().checked_sub(i)?;
        initrd[..e].ends_with(MAGIC).then(|| e - MAGIC.len())
    })?;
    let hdr = initrd.get(end.checked_sub(8)?..end)?;
    let size = u32::from_le_bytes(hdr[..4].try_into().unwrap()) as usize;
    let csum = u32::from_le_bytes(hdr[4..].try_into().unwrap());
    let start = (end - 8).checked_sub(size)?;
    let data = &initrd[start..end - 8];
    if checksum(data) != csum {
        return None;
    }
    let text = data.iter().rposition(|&b| b != 0).map_or(&data[..0], |p| &data[..=p]);
    Some((start, text))
}

/// Splits a kernel command line into parameters like the kernel's
/// `next_arg`: separated by spaces, with double quotes grouping
/// (they stay in the parameter).
pub fn split_cmdline(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut start, mut quoted) = (None, false);
    for (i, c) in s.char_indices() {
        if c.is_whitespace() && !quoted {
            if let Some(st) = start.take() {
                out.push(&s[st..i]);
            }
            continue;
        }
        start.get_or_insert(i);
        if c == '"' {
            quoted = !quoted;
        }
    }
    if let Some(st) = start {
        out.push(&s[st..]);
    }
    out
}

/// Key and value of a `key=value` parameter (value without the
/// outer quotes, as the kernel strips them); `None` as the value if
/// `=` is missing.
pub fn key_value(param: &str) -> (&str, Option<&str>) {
    match param.split_once('=') {
        Some((k, v)) => {
            let v = v.strip_prefix('"').map_or(v, |v| v.strip_suffix('"').unwrap_or(v));
            (k, Some(v))
        }
        None => (param, None),
    }
}

/// A bootconfig line for the parameter: `key = "value"` (in
/// double quotes, so commas, `#`, `;` and spaces stay in the value; single quotes if the
/// value contains double quotes).
pub fn param_line(param: &str) -> Result<String, String> {
    let (key, value) = key_value(param);
    let valid_key = !key.is_empty()
        && key
            .split('.')
            .all(|w| !w.is_empty() && w.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
    if !valid_key {
        return Err(format!("invalid bootconfig key: {key:?}"));
    }
    let value = value.unwrap_or("");
    let quote = match (value.contains('"'), value.contains('\'')) {
        (false, _) => '"',
        (true, false) => '\'',
        (true, true) => {
            return Err(format!("bootconfig value with both single and double quotes: {param:?}"));
        }
    };
    if value.contains('\n') {
        return Err(format!("multi-line bootconfig value: {param:?}"));
    }
    Ok(format!("{key} = {quote}{value}{quote}\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bytes produced by the kernel's `tools/bootconfig -a` from kernel 6.18.53 (built
    /// from the guest kernel sources) on the same inputs.
    #[test]
    fn blocco_come_tools_bootconfig() {
        // initrd "12345", bootconfig "a = 1\n": 5 + 7 + 8 + 12 = 32, no
        // padding.
        let mut initrd = b"12345".to_vec();
        append(&mut initrd, b"a = 1\n").unwrap();
        let want: &[u8] = &[
            0x31, 0x32, 0x33, 0x34, 0x35, 0x61, 0x20, 0x3d, 0x20, 0x31, 0x0a, 0x00, 0x07, 0x00, 0x00, 0x00,
            0x19, 0x01, 0x00, 0x00, 0x23, 0x42, 0x4f, 0x4f, 0x54, 0x43, 0x4f, 0x4e, 0x46, 0x49, 0x47, 0x0a,
        ];
        assert_eq!(initrd, want);
        assert_eq!(split(&initrd), Some((5, &b"a = 1\n"[..])));
        // initrd "1234567", two lines: 7 + 55 + 20 = 82, two NULs of
        // padding, size 0x39.
        let text = b"androidboot.hardware = \"vetro\"\nandroidboot.x = \"a, b\"\n";
        let mut initrd = b"1234567".to_vec();
        append(&mut initrd, text).unwrap();
        let mut want = b"1234567".to_vec();
        want.extend_from_slice(text);
        want.extend_from_slice(&[0, 0, 0, 0x39, 0, 0, 0, 0x21, 0x12, 0, 0]);
        want.extend_from_slice(b"#BOOTCONFIG\n");
        assert_eq!(initrd, want);
        assert_eq!(split(&initrd), Some((7, &text[..])));
    }

    #[test]
    fn riempimento_a_quattro_byte() {
        for n in 0..8 {
            let mut initrd = vec![0x55; n];
            append(&mut initrd, b"androidboot.x = \"y\"\n").unwrap();
            assert_eq!(initrd.len() % 4, 0, "initrd of {n} bytes");
            let (start, text) = split(&initrd).unwrap();
            assert_eq!(start, n);
            assert_eq!(text, b"androidboot.x = \"y\"\n");
            // The kernel finds the magic even after 1-3 bytes of alignment.
            initrd.extend_from_slice(&[0; 3]);
            assert_eq!(split(&initrd).unwrap().0, n);
        }
    }

    #[test]
    fn checksum_sbagliato_o_troppo_grande() {
        let mut initrd = Vec::new();
        append(&mut initrd, b"a = 1\n").unwrap();
        initrd[0] ^= 1;
        assert_eq!(split(&initrd), None);
        assert_eq!(split(b"niente"), None);
        let big = vec![b'x'; MAX_SIZE];
        assert!(append(&mut Vec::new(), &big).is_err());
    }

    #[test]
    fn riga_di_comando() {
        assert_eq!(
            split_cmdline("  console=ttyAMA0 foo=\"a b\"  bar\tandroidboot.x=1 "),
            ["console=ttyAMA0", "foo=\"a b\"", "bar", "androidboot.x=1"]
        );
        assert_eq!(key_value("foo=\"a b\""), ("foo", Some("a b")));
        assert_eq!(key_value("bar"), ("bar", None));
        assert_eq!(key_value("k=v=w"), ("k", Some("v=w")));
    }

    #[test]
    fn righe_di_bootconfig() {
        assert_eq!(param_line("androidboot.hardware=vetro").unwrap(), "androidboot.hardware = \"vetro\"\n");
        assert_eq!(param_line("androidboot.a=\"x, y # z\"").unwrap(), "androidboot.a = \"x, y # z\"\n");
        assert_eq!(param_line("androidboot.q=a\"b").unwrap(), "androidboot.q = 'a\"b'\n");
        assert_eq!(param_line("androidboot.flag").unwrap(), "androidboot.flag = \"\"\n");
        assert!(param_line("androidboot..x=1").is_err());
        assert!(param_line("androidboot.x/y=1").is_err());
        assert!(param_line("androidboot.x=a\"b'c").is_err());
    }
}

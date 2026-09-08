use crate::exec_policy::ExecShell;
use anyhow::Result;
use serde_json::Value;

/// Longest prefix within a byte budget, preserving complete UTF-8 characters.
pub(super) fn byte_prefix(text: &str, max_bytes: usize) -> &str {
    let mut end = max_bytes.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutputEncoding {
    Auto,
    Utf8,
    Oem,
}

struct DecodedChunk {
    text: String,
    consumed_bytes: usize,
}

impl OutputEncoding {
    pub(crate) fn supported_values() -> &'static [&'static str] {
        #[cfg(windows)]
        {
            &["auto", "utf8", "oem"]
        }
        #[cfg(not(windows))]
        {
            &["auto", "utf8"]
        }
    }

    pub(crate) fn parse(value: Option<&Value>) -> Result<Self> {
        match value {
            None => Ok(Self::Auto),
            Some(Value::String(value)) if value.eq_ignore_ascii_case("auto") => Ok(Self::Auto),
            Some(Value::String(value)) if value.eq_ignore_ascii_case("utf8") => Ok(Self::Utf8),
            #[cfg(windows)]
            Some(Value::String(value)) if value.eq_ignore_ascii_case("oem") => Ok(Self::Oem),
            _ => anyhow::bail!(
                "output_encoding must be 'auto', 'utf8', or 'oem' (oem requires Windows)"
            ),
        }
    }

    /// PowerShell is configured for UTF-8; Unix output retains the UTF-8
    /// default. cmd.exe built-ins may instead write the system OEM encoding.
    pub(crate) fn for_shell(self, shell: ExecShell) -> Self {
        if self != Self::Auto {
            return self;
        }
        #[cfg(windows)]
        if shell == ExecShell::PowerShell {
            return Self::Utf8;
        }
        #[cfg(not(windows))]
        let _ = shell;
        #[cfg(windows)]
        return Self::Auto;
        #[cfg(not(windows))]
        return Self::Utf8;
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Utf8 => "utf8",
            Self::Oem => "oem",
        }
    }

    /// Use the same decoder for complete foreground output and pipe streams.
    pub(crate) fn decode(self, bytes: &[u8]) -> String {
        OutputDecoder::new(self).push(bytes, true)
    }
}

#[cfg(windows)]
const MAX_AUTO_LINE_BYTES: usize = 64 * 1024;

/// One decoder per pipe: chunk boundaries and stdout/stderr interleaving must
/// not change how a character is decoded. Only complete UTF-8 enters the ring.
pub(crate) struct OutputDecoder {
    encoding: OutputEncoding,
    pending: Vec<u8>,
    #[cfg(windows)]
    code_page: u32,
    #[cfg(windows)]
    auto_line_encoding: Option<OutputEncoding>,
}

impl OutputDecoder {
    pub(crate) fn new(encoding: OutputEncoding) -> Self {
        Self {
            encoding,
            pending: Vec::new(),
            #[cfg(windows)]
            code_page: windows_oem_code_page(),
            #[cfg(windows)]
            auto_line_encoding: None,
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8], final_chunk: bool) -> String {
        #[cfg(windows)]
        if self.encoding == OutputEncoding::Auto {
            return self.push_auto(bytes, final_chunk);
        }

        self.pending.extend_from_slice(bytes);
        let chunk = self.decode_pending(self.encoding, final_chunk);
        self.pending.drain(..chunk.consumed_bytes);
        chunk.text
    }

    fn decode_pending(&self, encoding: OutputEncoding, final_chunk: bool) -> DecodedChunk {
        #[cfg(windows)]
        if encoding == OutputEncoding::Oem {
            return decode_windows_code_page_chunk(&self.pending, self.code_page, final_chunk);
        }
        #[cfg(not(windows))]
        let _ = encoding;
        decode_utf8_chunk(&self.pending, final_chunk)
    }

    #[cfg(windows)]
    fn flush_auto(&mut self, end_of_line: bool) -> String {
        let encoding = *self.auto_line_encoding.get_or_insert_with(|| {
            match std::str::from_utf8(&self.pending) {
                Ok(_) => OutputEncoding::Utf8,
                Err(error) if !end_of_line && error.error_len().is_none() => OutputEncoding::Utf8,
                Err(_) => OutputEncoding::Oem,
            }
        });
        let chunk = self.decode_pending(encoding, end_of_line);
        self.pending.drain(..chunk.consumed_bytes);
        if end_of_line {
            self.auto_line_encoding = None;
        }
        chunk.text
    }

    #[cfg(windows)]
    fn push_auto(&mut self, bytes: &[u8], final_chunk: bool) -> String {
        let mut text = String::new();
        for mut part in bytes.split_inclusive(|byte| *byte == b'\n') {
            while !part.is_empty() {
                // ASCII prompts need no guessing or newline buffering.
                if self.pending.is_empty() && self.auto_line_encoding.is_none() {
                    let ascii = part.iter().take_while(|byte| byte.is_ascii()).count();
                    text.push_str(std::str::from_utf8(&part[..ascii]).expect("ASCII prefix"));
                    part = &part[ascii..];
                    if part.is_empty() {
                        break;
                    }
                }
                let count = part.len().min(MAX_AUTO_LINE_BYTES - self.pending.len());
                self.pending.extend_from_slice(&part[..count]);
                part = &part[count..];
                let end_of_line = self.pending.ends_with(b"\n");
                if end_of_line || self.pending.len() == MAX_AUTO_LINE_BYTES {
                    text.push_str(&self.flush_auto(end_of_line));
                }
            }
        }
        // Guess once per line. Long lines lock their encoding at the buffer
        // limit and then stream; explicit utf8/oem never waits for a newline.
        if final_chunk || self.auto_line_encoding.is_some() {
            text.push_str(&self.flush_auto(final_chunk));
        }
        text
    }
}

fn decode_utf8_chunk(bytes: &[u8], final_chunk: bool) -> DecodedChunk {
    let mut consumed_bytes = 0;
    // Even after an invalid byte, preserve an incomplete trailing character.
    while consumed_bytes < bytes.len() {
        match std::str::from_utf8(&bytes[consumed_bytes..]) {
            Ok(_) => {
                consumed_bytes = bytes.len();
                break;
            }
            Err(error) => {
                consumed_bytes += error.valid_up_to();
                match error.error_len() {
                    Some(invalid) => consumed_bytes += invalid,
                    None => {
                        if final_chunk {
                            consumed_bytes = bytes.len();
                        }
                        break;
                    }
                }
            }
        }
    }
    DecodedChunk {
        text: String::from_utf8_lossy(&bytes[..consumed_bytes]).into_owned(),
        consumed_bytes,
    }
}

#[cfg(windows)]
pub(crate) fn windows_oem_code_page() -> u32 {
    unsafe { windows_sys::Win32::Globalization::GetOEMCP() }
}

#[cfg(windows)]
fn decode_windows_code_page_chunk(bytes: &[u8], code_page: u32, final_chunk: bool) -> DecodedChunk {
    use windows_sys::Win32::Globalization::IsDBCSLeadByteEx;

    // OEM code page 65001 is ordinary UTF-8 and can contain up to four-byte
    // sequences; IsDBCSLeadByteEx only models legacy one/two-byte code pages.
    if code_page == 65001 {
        return decode_utf8_chunk(bytes, final_chunk);
    }

    let mut consumed_bytes = bytes.len();
    if !final_chunk {
        let mut index = 0;
        while index < bytes.len() {
            let is_lead = unsafe { IsDBCSLeadByteEx(code_page, bytes[index]) } != 0;
            if is_lead {
                if index + 1 == bytes.len() {
                    consumed_bytes = index;
                    break;
                }
                index += 2;
            } else {
                index += 1;
            }
        }
    }

    DecodedChunk {
        text: decode_windows_code_page(&bytes[..consumed_bytes], code_page),
        consumed_bytes,
    }
}

#[cfg(windows)]
fn decode_windows_code_page(bytes: &[u8], code_page: u32) -> String {
    use windows_sys::Win32::Globalization::MultiByteToWideChar;

    if bytes.is_empty() {
        return String::new();
    }
    let Ok(byte_len) = i32::try_from(bytes.len()) else {
        return String::from_utf8_lossy(bytes).into_owned();
    };
    let wide_len = unsafe {
        MultiByteToWideChar(
            code_page,
            0,
            bytes.as_ptr(),
            byte_len,
            std::ptr::null_mut(),
            0,
        )
    };
    if wide_len <= 0 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let mut wide = vec![0u16; wide_len as usize];
    let written = unsafe {
        MultiByteToWideChar(
            code_page,
            0,
            bytes.as_ptr(),
            byte_len,
            wide.as_mut_ptr(),
            wide_len,
        )
    };
    if written <= 0 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    String::from_utf16_lossy(&wide[..written as usize])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_complete_characters_at_every_byte_budget() {
        for text in ["", "ascii", "中文", "🙂🚀", "aé中🙂z", "a\u{301}"] {
            for budget in 0..=text.len() + 1 {
                let expected: String = text
                    .chars()
                    .scan(0, |bytes, ch| {
                        *bytes += ch.len_utf8();
                        (*bytes <= budget).then_some(ch)
                    })
                    .collect();
                assert_eq!(byte_prefix(text, budget), expected);
            }
            assert_eq!(byte_prefix(text, usize::MAX), text);
        }
    }

    #[test]
    fn output_encoding_parse_and_utf8_auto_are_stable() {
        assert_eq!(OutputEncoding::parse(None).unwrap(), OutputEncoding::Auto);
        assert_eq!(
            OutputEncoding::parse(Some(&serde_json::json!("utf8"))).unwrap(),
            OutputEncoding::Utf8
        );
        assert_eq!(OutputEncoding::Auto.decode("中文🙂".as_bytes()), "中文🙂");
        assert!(OutputEncoding::parse(Some(&serde_json::json!("unknown"))).is_err());
    }

    #[cfg(not(windows))]
    #[test]
    fn non_windows_auto_is_utf8_and_does_not_advertise_oem() {
        assert_eq!(
            OutputEncoding::Auto.for_shell(ExecShell::Default),
            OutputEncoding::Utf8
        );
        assert_eq!(OutputEncoding::supported_values(), &["auto", "utf8"]);
        assert!(OutputEncoding::parse(Some(&serde_json::json!("oem"))).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn auto_decodes_utf8_and_cp936_lines_independently() {
        let mut bytes = "Node 中文🙂\r\n".as_bytes().to_vec();
        bytes.extend_from_slice(b"cmd \xd6\xd0\xce\xc4\r\n");
        for size in 1..=bytes.len() {
            let mut decoder = OutputDecoder::new(OutputEncoding::Auto);
            decoder.code_page = 936;
            let mut text = String::new();
            for part in bytes.chunks(size) {
                text.push_str(&decoder.push(part, false));
            }
            text.push_str(&decoder.push(&[], true));
            assert_eq!(text, "Node 中文🙂\r\ncmd 中文\r\n", "chunk size {size}");
        }
        assert_eq!(decode_windows_code_page(b"\xd6\xd0\xce\xc4", 936), "中文");
    }

    #[test]
    fn streaming_utf8_keeps_an_incomplete_character_for_the_next_chunk() {
        let bytes = "A中".as_bytes();
        let mut decoder = OutputDecoder::new(OutputEncoding::Utf8);
        assert_eq!(decoder.push(&bytes[..3], false), "A");
        assert_eq!(decoder.push(&bytes[3..], false), "中");
        assert_eq!(decoder.push(&[], true), "");
    }

    #[test]
    fn streaming_utf8_preserves_tails_after_invalid_bytes_and_flushes_eof() {
        let mut decoder = OutputDecoder::new(OutputEncoding::Utf8);
        assert_eq!(decoder.push(b"\xff\xe4\xb8", false), "�");
        assert_eq!(decoder.push(b"\xad\xf0\x9f", false), "中");
        assert_eq!(decoder.push(&[], true), "�");
        assert!(decoder.pending.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn auto_keeps_one_decision_per_line_and_flushes_cp936_at_eof() {
        for (bytes, expected) in [
            (b"\xd6\xd0\xc2\xa9".as_slice(), "中漏"),
            (b"\xe4\xb8".as_slice(), "涓"),
        ] {
            for encoding in [OutputEncoding::Auto, OutputEncoding::Oem] {
                for split in 0..=bytes.len() {
                    let mut decoder = OutputDecoder::new(encoding);
                    decoder.code_page = 936;
                    let mut text = decoder.push(&bytes[..split], false);
                    text.push_str(&decoder.push(&bytes[split..], false));
                    text.push_str(&decoder.push(&[], true));
                    assert_eq!(text, expected, "encoding {encoding:?}, split {split}");
                    assert!(decoder.pending.is_empty());
                }
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn auto_line_buffer_is_bounded_and_ascii_prompts_are_immediate() {
        let mut decoder = OutputDecoder::new(OutputEncoding::Auto);
        assert_eq!(decoder.push(b"prompt> ", false), "prompt> ");
        let expected = format!("{}\nend", "中🙂".repeat(MAX_AUTO_LINE_BYTES));
        let mut text = String::new();
        for part in expected.as_bytes().chunks(137) {
            text.push_str(&decoder.push(part, false));
            assert!(decoder.pending.len() < MAX_AUTO_LINE_BYTES);
        }
        text.push_str(&decoder.push(&[], true));
        assert_eq!(text, expected);
        assert_eq!(OutputEncoding::Auto.decode(expected.as_bytes()), expected);
    }

    #[cfg(windows)]
    #[test]
    fn streaming_cp936_keeps_a_trailing_lead_byte() {
        let first = decode_windows_code_page_chunk(b"A\xd6", 936, false);
        assert_eq!(first.text, "A");
        assert_eq!(first.consumed_bytes, 1);

        let second = decode_windows_code_page_chunk(b"\xd6\xd0", 936, false);
        assert_eq!(second.text, "中");
        assert_eq!(second.consumed_bytes, 2);
    }
}

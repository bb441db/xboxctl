//! The `PPAS` firmware image Pendragon devices take. The SDK sends the file
//! unmodified; only the header is understood here, enough to show and check
//! the version before sending the rest of the bytes as-is.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub version: [u16; 4],
    pub data: Vec<u8>,
}

const HEADER: usize = 0x20;

impl Image {
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < HEADER || &bytes[..4] != b"PPAS" {
            return Err("not a PPAS firmware image".to_owned());
        }
        let format_version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        if format_version != 2 {
            return Err(format!("unsupported PPAS format version {format_version}"));
        }
        let file_size = u32::from_le_bytes(bytes[0x14..0x18].try_into().unwrap()) as usize;
        if bytes.len() != file_size {
            return Err(format!(
                "file is {:#x} bytes; header says {file_size:#x}",
                bytes.len()
            ));
        }
        let version = std::array::from_fn(|i| {
            u16::from_le_bytes([bytes[0x0c + i * 2], bytes[0x0c + i * 2 + 1]])
        });
        Ok(Self {
            version,
            data: bytes.to_vec(),
        })
    }

    pub fn version_string(&self) -> String {
        let [a, b, c, d] = self.version;
        format!("{a}.{b}.{c}.{d}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(file_size_override: Option<u32>) -> Vec<u8> {
        let mut out = vec![0u8; HEADER];
        out[..4].copy_from_slice(b"PPAS");
        out[4..8].copy_from_slice(&2u32.to_le_bytes());
        out[0x0c..0x14].copy_from_slice(&[6, 0, 19, 0, 50, 0, 0, 0]);
        let size = file_size_override.unwrap_or(out.len() as u32);
        out[0x14..0x18].copy_from_slice(&size.to_le_bytes());
        out
    }

    #[test]
    fn parses_header() {
        let image = Image::parse(&sample(None)).unwrap();
        assert_eq!(image.version_string(), "6.19.50.0");
    }

    #[test]
    fn rejects_size_mismatch() {
        assert!(Image::parse(&sample(Some(1))).is_err());
    }

    #[test]
    fn rejects_wrong_magic() {
        let mut bytes = sample(None);
        bytes[0] = b'X';
        assert!(Image::parse(&bytes).is_err());
    }
}

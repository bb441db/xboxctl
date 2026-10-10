//! The `rdhm` firmware package used by the Xbox Accessories app.
//!
//! Header (little-endian): `rdhm`, u32 header version (1), u16 VID, u16 PID,
//! u16x4 package version, u16x4 minimum version, u32 entry count at 0x1c.
//! 0x40-byte entries follow from 0x40: u32 id, u32 flags, u32 file offset,
//! u32 size, u32 target address, u32 unused, u16x4 version, u16x4 minimum.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Version(pub [u16; 4]);

impl Version {
    pub fn from_le(bytes: &[u8]) -> Self {
        Self(std::array::from_fn(|i| {
            u16::from_le_bytes([bytes[i * 2], bytes[i * 2 + 1]])
        }))
    }

    pub fn to_le(self) -> [u8; 8] {
        let mut out = [0u8; 8];
        for (i, part) in self.0.iter().enumerate() {
            out[i * 2..i * 2 + 2].copy_from_slice(&part.to_le_bytes());
        }
        out
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b, c, d] = self.0;
        write!(f, "{a}.{b}.{c}.{d}")
    }
}

/// Entries with this flag are never written by the Windows updater.
pub const FLAG_CRITICAL: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Component {
    pub id: u32,
    pub flags: u32,
    pub target_address: u32,
    pub version: Version,
    pub data: Vec<u8>,
}

impl Component {
    pub fn is_critical(&self) -> bool {
        self.flags & FLAG_CRITICAL != 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub vendor_id: u16,
    pub product_id: u16,
    pub version: Version,
    pub components: Vec<Component>,
}

const HEADER: usize = 0x40;
const ENTRY: usize = 0x40;
const MAX_ENTRIES: usize = 10;

impl Package {
    /// Parse and validate a package with the same rules as the Windows host.
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < HEADER || &bytes[..4] != b"rdhm" {
            return Err("not an rdhm firmware package".to_owned());
        }
        let u32_at = |offset: usize| u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        if u32_at(4) != 1 {
            return Err(format!("unsupported rdhm header version {}", u32_at(4)));
        }
        let count = u32_at(0x1c) as usize;
        if count == 0 || count > MAX_ENTRIES {
            return Err(format!("invalid component count {count}"));
        }
        let table_end = HEADER + count * ENTRY;
        if bytes.len() < table_end {
            return Err("truncated component table".to_owned());
        }

        let mut components = Vec::with_capacity(count);
        let mut expected_len = table_end;
        for index in 0..count {
            let entry = HEADER + index * ENTRY;
            let offset = u32_at(entry + 8) as usize;
            let size = u32_at(entry + 0x0c) as usize;
            let data = offset
                .checked_add(size)
                .and_then(|end| bytes.get(offset..end))
                .ok_or_else(|| format!("component {index} lies outside the file"))?;
            expected_len += size;
            components.push(Component {
                id: u32_at(entry),
                flags: u32_at(entry + 4),
                target_address: u32_at(entry + 0x10),
                version: Version::from_le(&bytes[entry + 0x18..entry + 0x20]),
                data: data.to_vec(),
            });
        }
        if bytes.len() != expected_len {
            return Err(format!(
                "file is {:#x} bytes; header and components describe {expected_len:#x}",
                bytes.len()
            ));
        }

        Ok(Self {
            vendor_id: u16::from_le_bytes([bytes[8], bytes[9]]),
            product_id: u16::from_le_bytes([bytes[10], bytes[11]]),
            version: Version::from_le(&bytes[0x0c..0x14]),
            components,
        })
    }

    /// Build a package; used by tests and tooling.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = vec![0u8; HEADER + self.components.len() * ENTRY];
        out[..4].copy_from_slice(b"rdhm");
        out[4..8].copy_from_slice(&1u32.to_le_bytes());
        out[8..10].copy_from_slice(&self.vendor_id.to_le_bytes());
        out[10..12].copy_from_slice(&self.product_id.to_le_bytes());
        out[0x0c..0x14].copy_from_slice(&self.version.to_le());
        out[0x1c..0x20].copy_from_slice(&(self.components.len() as u32).to_le_bytes());
        for (index, component) in self.components.iter().enumerate() {
            let entry = HEADER + index * ENTRY;
            let offset = out.len() as u32;
            out[entry..entry + 4].copy_from_slice(&component.id.to_le_bytes());
            out[entry + 4..entry + 8].copy_from_slice(&component.flags.to_le_bytes());
            out[entry + 8..entry + 0x0c].copy_from_slice(&offset.to_le_bytes());
            out[entry + 0x0c..entry + 0x10]
                .copy_from_slice(&(component.data.len() as u32).to_le_bytes());
            out[entry + 0x10..entry + 0x14].copy_from_slice(&component.target_address.to_le_bytes());
            out[entry + 0x18..entry + 0x20].copy_from_slice(&component.version.to_le());
            out.extend_from_slice(&component.data);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_validates_size() {
        let package = Package {
            vendor_id: 0x045e,
            product_id: 0x0b12,
            version: Version([5, 23, 6, 0]),
            components: vec![Component {
                id: 3,
                flags: 0,
                target_address: 0x1002_7000,
                version: Version([5, 23, 6, 0]),
                data: vec![0xaa; 0x100],
            }],
        };
        let mut bytes = package.to_bytes();
        assert_eq!(Package::parse(&bytes).unwrap(), package);
        bytes.push(0);
        assert!(Package::parse(&bytes).is_err());
    }
}

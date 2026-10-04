//! Assemble PNG icon representations into an ICNS file without platform GUI services.
use std::{fs, io, path::Path};
const ENTRIES: [(&[u8; 4], &str); 11] = [
    (b"icp4", "16x16"),
    (b"icp5", "32x32"),
    (b"icp6", "32x32@2x"),
    (b"ic07", "128x128"),
    (b"ic08", "256x256"),
    (b"ic09", "512x512"),
    (b"ic10", "512x512@2x"),
    (b"ic11", "16x16@2x"),
    (b"ic12", "32x32@2x"),
    (b"ic13", "128x128@2x"),
    (b"ic14", "256x256@2x"),
];
fn pack(iconset: &Path) -> io::Result<Vec<u8>> {
    let mut data = b"icns\0\0\0\0".to_vec();
    for (kind, name) in ENTRIES {
        let png = fs::read(iconset.join(format!("icon_{name}.png")))?;
        if !png.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Icon is not a PNG",
            ));
        }
        let length = u32::try_from(png.len() + 8)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Icon is too large"))?;
        data.extend(kind);
        data.extend(length.to_be_bytes());
        data.extend(png);
    }
    let length = u32::try_from(data.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "ICNS is too large"))?;
    data[4..8].copy_from_slice(&length.to_be_bytes());
    Ok(data)
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 2 {
        return Err("Usage: pack_icns ICONSET OUTPUT.icns".into());
    }
    let data = pack(Path::new(&args[0]))?;
    fs::write(&args[1], data)?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chunks_have_correct_identifiers_lengths_and_payloads() {
        let dir = tempfile::tempdir().unwrap();
        for (_, name) in ENTRIES {
            fs::write(
                dir.path().join(format!("icon_{name}.png")),
                b"\x89PNG\r\n\x1a\nexample",
            )
            .unwrap();
        }
        let data = pack(dir.path()).unwrap();
        assert_eq!(&data[..4], b"icns");
        assert_eq!(
            u32::from_be_bytes(data[4..8].try_into().unwrap()) as usize,
            data.len()
        );
        let mut offset = 8;
        for (kind, _) in ENTRIES {
            assert_eq!(&data[offset..offset + 4], kind);
            let len = u32::from_be_bytes(data[offset + 4..offset + 8].try_into().unwrap()) as usize;
            assert_eq!(&data[offset + 8..offset + len], b"\x89PNG\r\n\x1a\nexample");
            offset += len;
        }
        assert_eq!(offset, data.len());
        fs::write(dir.path().join("icon_16x16.png"), "not PNG").unwrap();
        assert!(pack(dir.path()).is_err());
    }
}

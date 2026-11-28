use std::fs::File;
use std::io::Read;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerType {
    Ts,
    Fmp4,
    Unknown,
}

impl ContainerType {
    pub fn segment_ext(self) -> &'static str {
        match self {
            Self::Fmp4 => ".m4s",
            Self::Ts | Self::Unknown => ".ts",
        }
    }
}

pub fn detect_container_type(url: &str) -> ContainerType {
    let clean = url.split(['?', '#']).next().unwrap_or(url);
    let ext = Path::new(clean)
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| format!(".{}", s.to_ascii_lowercase()));
    match ext.as_deref() {
        Some(
            ".m4s" | ".mp4" | ".m4v" | ".m4a" | ".cmfv" | ".cmfa" | ".cmft" | ".ismv" | ".isma",
        ) => ContainerType::Fmp4,
        Some(".ts" | ".m2ts" | ".mts") => ContainerType::Ts,
        _ => ContainerType::Unknown,
    }
}

pub fn detect_container_by_header(buf: &[u8]) -> ContainerType {
    if buf.len() >= 8 {
        let box_type = &buf[4..8];
        if matches!(
            box_type,
            b"ftyp" | b"styp" | b"moov" | b"moof" | b"sidx" | b"ssix" | b"uuid" | b"free" | b"skip"
        ) {
            return ContainerType::Fmp4;
        }
    }
    if buf.len() >= 192 && buf[0] == 0x47 && buf[188] == 0x47 {
        return ContainerType::Ts;
    }
    ContainerType::Unknown
}

pub fn detect_container_by_file(path: &Path) -> ContainerType {
    let Ok(mut f) = File::open(path) else {
        return ContainerType::Unknown;
    };
    let mut buf = [0u8; 256];
    let Ok(n) = f.read(&mut buf) else {
        return ContainerType::Unknown;
    };
    detect_container_by_header(&buf[..n])
}

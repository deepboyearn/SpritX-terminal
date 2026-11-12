//! Bounded, read-only file loading for the pane-local preview. No shell commands.
use std::{
    fs,
    io::{self, Cursor, Read},
    path::Path,
};

pub const TEXT_LIMIT: usize = 64 * 1024;
const IMAGE_LIMIT: usize = 8 * 1024 * 1024;

pub enum Content {
    Text {
        text: String,
        truncated: bool,
    },
    Image {
        png: Vec<u8>,
        width: u32,
        height: u32,
    },
    Unsupported,
}

pub fn load(path: &Path) -> io::Result<Content> {
    if !fs::metadata(path)?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Only regular files can be previewed",
        ));
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Only regular files can be previewed",
        ));
    }
    let extension = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_image = matches!(
        extension.as_str(),
        "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp"
    );
    let limit = if is_image { IMAGE_LIMIT } else { TEXT_LIMIT };
    if is_image && metadata.len() > IMAGE_LIMIT as u64 {
        return Err(io::Error::other("Image preview is limited to 8 MB"));
    }
    let mut bytes = Vec::new();
    file.take((limit + 1) as u64).read_to_end(&mut bytes)?;
    let truncated = bytes.len() > limit;
    bytes.truncate(limit);
    if is_image {
        if truncated {
            return Err(io::Error::other("Image preview is limited to 8 MB"));
        }
        let reader = image::ImageReader::new(Cursor::new(&bytes)).with_guessed_format()?;
        let (width, height) = reader.into_dimensions().map_err(io::Error::other)?;
        if u64::from(width) * u64::from(height) > 16_000_000 {
            return Err(io::Error::other("Image is too large to preview safely"));
        }
        let mut reader = image::ImageReader::new(Cursor::new(&bytes)).with_guessed_format()?;
        let mut limits = image::Limits::default();
        limits.max_alloc = Some(64 * 1024 * 1024);
        reader.limits(limits);
        let image = reader
            .decode()
            .map_err(io::Error::other)?
            .thumbnail(1600, 1600);
        let mut png = Cursor::new(Vec::new());
        image
            .write_to(&mut png, image::ImageFormat::Png)
            .map_err(io::Error::other)?;
        return Ok(Content::Image {
            png: png.into_inner(),
            width,
            height,
        });
    }
    let text = if bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]) {
        let little = bytes[0] == 0xff;
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|p| {
                if little {
                    u16::from_le_bytes([p[0], p[1]])
                } else {
                    u16::from_be_bytes([p[0], p[1]])
                }
            })
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        if bytes
            .iter()
            .any(|&b| b == 0 || (b < 0x20 && !matches!(b, b'\n' | b'\r' | b'\t' | 0x0c)))
        {
            return Ok(Content::Unsupported);
        }
        // A byte cap may split a UTF-8 character; omit that incomplete tail.
        if truncated {
            if let Err(error) = std::str::from_utf8(&bytes) {
                if error.error_len().is_none() {
                    bytes.truncate(error.valid_up_to());
                }
            }
        }
        String::from_utf8_lossy(&bytes)
            .trim_start_matches('\u{feff}')
            .to_string()
    };
    Ok(Content::Text { text, truncated })
}

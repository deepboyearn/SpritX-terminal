use spritx_terminal::backend::file_preview::{load, Content, TEXT_LIMIT};
use std::path::PathBuf;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "spritx-preview-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn code_unicode_spaces_and_empty_files_remain_read_only() {
    let f = Fixture::new();
    let code = "fn main() {\n\tprintln!(\"हैलो 🦀\");\n}\n";
    let path = f.file("code with spaces.rs", code.as_bytes());
    let Content::Text { text, truncated } = load(&path).unwrap() else {
        panic!("not text")
    };
    assert_eq!(text, code);
    assert!(!truncated);
    assert_eq!(std::fs::read_to_string(path).unwrap(), code);
    let Content::Text { text, .. } = load(&f.file("empty.txt", b"")).unwrap() else {
        panic!("empty file")
    };
    assert!(text.is_empty());
}

#[test]
fn preview_caps_large_logs_without_splitting_a_utf8_character() {
    let f = Fixture::new();
    let mut text = "x".repeat(TEXT_LIMIT - 1);
    text.push_str("🦀more");
    let Content::Text { text, truncated } = load(&f.file("large.log", text.as_bytes())).unwrap()
    else {
        panic!("not text")
    };
    assert!(truncated);
    assert_eq!(text.len(), TEXT_LIMIT - 1);
    assert!(!text.contains('\u{fffd}'));
}

#[test]
fn windows_utf16_text_decodes_and_binary_files_are_not_rendered_as_code() {
    let f = Fixture::new();
    let mut bytes = vec![0xff, 0xfe];
    for unit in "Windows ✓".encode_utf16() {
        bytes.extend(unit.to_le_bytes());
    }
    let Content::Text { text, .. } = load(&f.file("utf16.txt", &bytes)).unwrap() else {
        panic!("UTF-16")
    };
    assert_eq!(text, "Windows ✓");
    assert!(matches!(
        load(&f.file("data.bin", b"binary\0\x01")).unwrap(),
        Content::Unsupported
    ));
    assert!(load(&f.0).is_err());
    assert!(load(&f.0.join("missing")).is_err());
}

#[test]
fn png_preview_preserves_colors_and_rejects_invalid_images() {
    let f = Fixture::new();
    let path = f.0.join("image.png");
    image::RgbaImage::from_pixel(24, 16, image::Rgba([32, 180, 240, 255]))
        .save(&path)
        .unwrap();
    let Content::Image { png, width, height } = load(&path).unwrap() else {
        panic!("image")
    };
    assert_eq!((width, height), (24, 16));
    assert_eq!(
        image::load_from_memory(&png)
            .unwrap()
            .into_rgba8()
            .get_pixel(0, 0)
            .0,
        [32, 180, 240, 255]
    );
    assert!(load(&f.file("broken.png", b"not an image")).is_err());
}

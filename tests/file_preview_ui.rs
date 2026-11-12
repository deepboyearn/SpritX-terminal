#[test]
#[ignore = "requires a display; run with --ignored"]
fn previews_slide_and_stay_local_to_the_clicked_terminal() {
    let root = std::env::temp_dir().join(format!("spritx-preview-ui-{}", std::process::id()));
    std::fs::create_dir_all(root.join("fixture")).unwrap();
    let fixture = root.join("fixture");
    std::fs::write(fixture.join("first.rs"), "first\n").unwrap();
    std::fs::write(fixture.join("final file.rs"), "fn final_preview() {}\n").unwrap();
    image::RgbaImage::from_pixel(24, 16, image::Rgba([32, 180, 240, 255]))
        .save(fixture.join("picture.png"))
        .unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_spritx-terminal"))
        .env(
            if cfg!(windows) {
                "APPDATA"
            } else {
                "XDG_CONFIG_HOME"
            },
            &root,
        )
        .env("SPRITX_SMOKE", "1")
        .env("SPRITX_QUIET", "1")
        .env("SPRITX_FILE_PREVIEW_TEST", "1")
        .env("SPRITX_PREVIEW_FIXTURE", &fixture)
        .spawn()
        .unwrap();
    let started = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > std::time::Duration::from_secs(60) {
            let _ = child.kill();
            panic!("File preview lifecycle timed out");
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
    };
    assert!(status.success(), "preview lifecycle exited with {status}");
    let _ = std::fs::remove_dir_all(root);
}

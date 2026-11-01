//! The pairing QR must encode the pairing URL. It used to be a hardcoded
//! bitmap, which looked scannable but carried no data at all.

use spritx_terminal::screens::overlays::qr_modules;

#[test]
fn a_pairing_url_encodes_to_a_symbol() {
    let (width, dark) = qr_modules("http://192.168.1.10:4530/#p=abcdefgh").expect("encodes");
    assert_eq!(dark.len(), width * width, "matrix is width x width");
    assert!(width >= 21 && width <= 177, "plausible QR width: {width}");
}

#[test]
fn the_matrix_follows_the_url() {
    let (_, a) = qr_modules("http://10.0.0.5:4530/#p=one").unwrap();
    let (_, b) = qr_modules("http://10.0.0.5:4530/#p=two").unwrap();
    assert_ne!(a, b, "different secrets must not draw the same code");
}

#[test]
fn the_three_position_markers_are_present() {
    let (width, dark) = qr_modules("http://10.0.0.5:4530/#p=one").unwrap();
    // Every QR symbol has 7x7 finder patterns in three corners, and the fourth
    // corner is data. A decorative grid has no reason to obey this.
    for (top, left) in [(0, 0), (0, width - 7), (width - 7, 0)] {
        for row in 0..7 {
            for col in 0..7 {
                let ring = row == 0 || row == 6 || col == 0 || col == 6;
                let core = (2..5).contains(&row) && (2..5).contains(&col);
                assert_eq!(
                    dark[(top + row) * width + left + col],
                    ring || core,
                    "finder pattern at ({top},{left}) differs at ({row},{col})"
                );
            }
        }
    }
    let blank = (0..7).all(|row| (0..7).all(|col| !dark[(width - 7 + row) * width + col]));
    assert!(
        !blank,
        "the bottom-right corner carries data, not another finder pattern"
    );
}

#[test]
fn an_unencodable_url_yields_no_code() {
    assert_eq!(qr_modules(""), None, "nothing to encode");
    assert!(
        qr_modules(&"x".repeat(5000)).is_none(),
        "too long to encode must fail rather than draw something bogus"
    );
}

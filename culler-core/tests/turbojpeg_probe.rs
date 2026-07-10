//! Linking probe: proves the `turbojpeg` crate links the SYSTEM libjpeg-turbo
//! (pkg-config; this machine has no cmake/nasm so a from-source build cannot
//! have produced this binary). Kept as a permanent guard for the dep spelling.

#[test]
fn turbojpeg_system_library_round_trip() {
    // 8x8 RGBA gradient -> JPEG -> decode header; dims must survive.
    let w = 8usize;
    let h = 8usize;
    let mut px = vec![0u8; w * h * 4];
    for i in 0..w * h {
        px[i * 4] = (i * 4) as u8;
        px[i * 4 + 1] = 128;
        px[i * 4 + 2] = 64;
        px[i * 4 + 3] = 255;
    }
    let image = turbojpeg::Image {
        pixels: px.as_slice(),
        width: w,
        pitch: w * 4,
        height: h,
        format: turbojpeg::PixelFormat::RGBA,
    };
    let jpeg = turbojpeg::compress(image, 95, turbojpeg::Subsamp::Sub2x2).expect("compress");
    let decoded = turbojpeg::decompress(&jpeg, turbojpeg::PixelFormat::RGBA).expect("decompress");
    assert_eq!((decoded.width, decoded.height), (w, h));
    assert_eq!(decoded.pixels.len(), w * h * 4);
}

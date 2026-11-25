fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    #[cfg(windows)]
    if let Err(err) = build_windows_resources() {
        panic!("Failed to compile Windows icon/version resources: {err}");
    }
}

#[cfg(windows)]
fn build_windows_resources() -> Result<(), Box<dyn std::error::Error>> {
    use std::path::PathBuf;

    let out_dir = PathBuf::from(std::env::var("OUT_DIR")?);
    let ico_path = out_dir.join("jarvis.ico");

    let ico_bytes = generate_arc_reactor_ico(&[256, 64, 32, 16]);
    std::fs::write(&ico_path, ico_bytes)?;

    let ico_str = ico_path
        .to_str()
        .ok_or("OUT_DIR path is not valid UTF-8")?;

    let mut res = winresource::WindowsResource::new();
    res.set_icon(ico_str);
    res.set("ProductName", "JARVIS-RS");
    res.set("FileDescription", "JARVIS-RS AI Desktop Assistant");
    res.compile()?;

    Ok(())
}

#[cfg(windows)]
fn generate_arc_reactor_ico(sizes: &[u32]) -> Vec<u8> {
    let mut entries: Vec<(u32, Vec<u8>)> = Vec::with_capacity(sizes.len());
    for &size in sizes {
        entries.push((size, build_ico_dib_entry(size)));
    }

    let count = entries.len() as u16;
    let mut out = Vec::new();

    // ICONDIR header (6 bytes)
    out.extend_from_slice(&0u16.to_le_bytes()); // idReserved
    out.extend_from_slice(&1u16.to_le_bytes()); // idType (1 = icon)
    out.extend_from_slice(&count.to_le_bytes()); // idCount

    let mut offset = 6usize + entries.len() * 16;
    for (size, dib) in &entries {
        let dim_byte = if *size >= 256 { 0u8 } else { *size as u8 };
        out.push(dim_byte); // bWidth
        out.push(dim_byte); // bHeight
        out.push(0u8); // bColorCount
        out.push(0u8); // bReserved
        out.extend_from_slice(&1u16.to_le_bytes()); // wPlanes
        out.extend_from_slice(&32u16.to_le_bytes()); // wBitCount
        out.extend_from_slice(&(dib.len() as u32).to_le_bytes()); // dwBytesInRes
        out.extend_from_slice(&(offset as u32).to_le_bytes()); // dwImageOffset
        offset += dib.len();
    }

    for (_, dib) in entries {
        out.extend_from_slice(&dib);
    }

    out
}

#[cfg(windows)]
fn build_ico_dib_entry(size: u32) -> Vec<u8> {
    let xor_len = (size * size * 4) as usize;
    let and_row_bytes = (size as usize).div_ceil(32) * 4;
    let and_len = and_row_bytes * size as usize;

    let mut dib = Vec::with_capacity(40 + xor_len + and_len);

    // BITMAPINFOHEADER (40 bytes)
    dib.extend_from_slice(&40u32.to_le_bytes()); // biSize
    dib.extend_from_slice(&(size as i32).to_le_bytes()); // biWidth
    dib.extend_from_slice(&((size * 2) as i32).to_le_bytes()); // biHeight (XOR + AND)
    dib.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    dib.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    dib.extend_from_slice(&0u32.to_le_bytes()); // biCompression (BI_RGB)
    dib.extend_from_slice(&0u32.to_le_bytes()); // biSizeImage
    dib.extend_from_slice(&0i32.to_le_bytes()); // biXPelsPerMeter
    dib.extend_from_slice(&0i32.to_le_bytes()); // biYPelsPerMeter
    dib.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    dib.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant

    let mut and_mask = vec![0u8; and_len];
    let center = (size as f32 - 1.0) / 2.0;
    let radius = size as f32 * 0.46;

    // Bottom-up BGRA pixels + 32-bit aligned 1bpp AND mask
    for (row_idx, y) in (0..size).rev().enumerate() {
        for x in 0..size {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let norm = (dx * dx + dy * dy).sqrt() / radius;

            let (b, g, r, a) = if norm <= 1.0 {
                // Dark navy disc base (#060C16)
                let (mut r, mut g, mut b, mut a) = (6u8, 12u8, 22u8, 245u8);

                if (0.78..=0.94).contains(&norm) {
                    // Outer ring: Cyan Neon #00F0FF
                    (r, g, b, a) = (0, 240, 255, 255);
                } else if (0.54..=0.66).contains(&norm) {
                    // Middle ring: Cyan #00B8FF
                    (r, g, b, a) = (0, 184, 255, 255);
                } else if (0.30..=0.40).contains(&norm) {
                    // Inner ring: Cyan Neon #00F0FF
                    (r, g, b, a) = (0, 240, 255, 255);
                } else if norm <= 0.20 {
                    // Bright reactor core
                    (r, g, b, a) = (190, 252, 255, 255);
                }

                if norm > 0.94 {
                    let fade = ((1.0 - norm) / 0.06).clamp(0.0, 1.0);
                    a = (a as f32 * fade) as u8;
                }

                (b, g, r, a)
            } else {
                (0, 0, 0, 0)
            };

            dib.push(b);
            dib.push(g);
            dib.push(r);
            dib.push(a);

            if a == 0 {
                and_mask[row_idx * and_row_bytes + (x as usize / 8)] |= 0x80 >> (x % 8);
            }
        }
    }

    dib.extend_from_slice(&and_mask);
    dib
}

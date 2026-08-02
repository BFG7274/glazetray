//! Dev tool: render an ASCII map of a glazetray_frame.raw dump.
use std::io::Read;
fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| std::env::temp_dir().join("glazetray_frame.raw").to_string_lossy().into());
    let mut data = Vec::new();
    std::fs::File::open(&path).expect("open dump").read_to_end(&mut data).expect("read");
    let w = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
    let h = u32::from_le_bytes(data[4..8].try_into().unwrap()) as usize;
    println!("frame {w}x{h}");
    let px = &data[8..];
    let step = 8;
    for y in (0..h).step_by(step) {
        let mut line = String::new();
        for x in (0..w).step_by(step) {
            let i = (y * w + x) * 4;
            let (r, g, b, a) = (px[i], px[i + 1], px[i + 2], px[i + 3]);
            let ch = if a < 40 { '.' }
                else if r < 70 && g < 70 && b < 70 { '#' }
                else if b > 180 && r < 150 { 'A' }
                else if r > 200 && g > 200 && b > 200 { 'W' }
                else { '?' };
            line.push(ch);
        }
        println!("{line}");
    }
}

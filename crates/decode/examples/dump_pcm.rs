//! Throwaway diagnostic: decode a file and dump the packed S24In32High wire
//! bytes to stdout as raw little-endian i32 values for external comparison.
//! Run: cargo run -p sointty-decode --example dump_pcm -- <path>

use sointty_core::{Decoder, OutputSpec, DeviceFormat, pack_exact};

fn main() {
    let path = std::env::args().nth(1).expect("usage: dump_pcm <file>");
    let source = sointty_source::FileSource::open(&path).expect("open source");
    let ext = std::path::Path::new(&path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_owned);
    let mut decoder = sointty_decode::SymphoniaDecoder::open(Box::new(source), ext.as_deref())
        .expect("open decoder");
    let spec = decoder.spec();
    eprintln!("spec: {spec:?}");
    let output = OutputSpec {
        device: "diag".to_owned(),
        rate_hz: spec.rate_hz,
        layout: spec.layout,
        format: DeviceFormat::S24In32High,
        valid_bits: 24,
    };
    let mut bytes = Vec::new();
    while let Some(block) = decoder.next_block().expect("decode") {
        pack_exact(block, &output, &mut bytes).expect("pack");
        for chunk in bytes.chunks_exact(4) {
            let v = i32::from_le_bytes(chunk.try_into().unwrap());
            println!("{v}");
        }
    }
}

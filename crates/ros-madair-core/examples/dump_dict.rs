use std::fs;
use ros_madair_core::Dictionary;

fn main() {
    let base = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "example/static/index".to_string());

    let dict_bytes = fs::read(format!("{base}/dictionary.bin")).unwrap();
    let dict = Dictionary::from_bytes(&dict_bytes).unwrap();

    println!("Dictionary: {} terms", dict.len());
    for id in 0..dict.len() as u32 {
        if let Some(uri) = dict.resolve(id) {
            println!("{}\t{}", id, uri);
        }
    }
}

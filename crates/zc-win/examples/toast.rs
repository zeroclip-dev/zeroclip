//! Shows one toast for two seconds: `cargo run -p zc-win --example toast`.
#![allow(missing_docs, clippy::print_stdout)]

use std::time::Duration;

fn main() {
    let r = zc_win::toast::show(zc_win::toast::Toast {
        title: "Clipped last 20 min".into(),
        detail: Some("Saved to Clips".into()),
        accent: 0x2dd4bf,
        background: 0x18181b,
        border: 0x27272a,
        text: 0xfafafa,
        muted: 0xa1a1aa,
        hold: Duration::from_millis(1500),
        fade: Duration::from_millis(250),
    });
    println!("{r:?}");
    std::thread::sleep(Duration::from_secs(3));
}

//! Manual check for the GPU video pipeline on a real machine:
//! `cargo run -p zc-video --example smoke`.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().init();
    let _apartment = zc_win::com::Apartment::multithreaded()?;
    let (_device, _context) = zc_video::device::create_device()?;
    Ok(())
}

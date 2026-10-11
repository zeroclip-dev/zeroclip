//! Muxes an Annex-B H.264 byte stream into fragmented MP4 and tests the output.
//!
//! Run with: `cargo run -p zc-mux --example annexb_to_fmp4 -- input.h264 output.mp4`

#![allow(clippy::print_stdout, reason = "dev tool")]

use std::env;
use std::fs::File;
use std::io::{self, Read};
use std::process;

use zc_core::media::{Codec, ContainerFormat, Packet, PacketMeta, TrackId, TrackInfo, TrackKind};
use zc_core::time::Hns;
use zc_mux::bitstream;
use zc_mux::container;

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: {} <input.h264> <output.mp4>", args[0]);
        process::exit(1);
    }

    let input_path = &args[1];
    let output_path = &args[2];

    let mut input_file = File::open(input_path)?;
    let mut data = Vec::new();
    input_file.read_to_end(&mut data)?;

    // Split NAL units and group into access units at AUD (type 9).
    let mut access_units: Vec<Vec<u8>> = Vec::new();
    let mut current_au: Vec<u8> = Vec::new();

    for nal in bitstream::nal_units(&data) {
        let nal_type = nal.first().map_or(0, |b| b & 0x1F);
        if nal_type == 9 && !current_au.is_empty() {
            access_units.push(std::mem::take(&mut current_au));
        }
        // Write start code and nal unit
        current_au.extend_from_slice(&[0, 0, 0, 1]);
        current_au.extend_from_slice(nal);
    }
    if !current_au.is_empty() {
        access_units.push(current_au);
    }

    println!("Read {} access units from {}", access_units.len(), input_path);

    let tracks = [TrackInfo {
        track: TrackId::Video,
        label: "Video".into(),
        kind: TrackKind::Video { codec: Codec::H264, width: 640, height: 360, fps: 30 },
        codec_private: Vec::new(),
    }];

    let out_file = File::create(output_path)?;
    let mut writer =
        container::create(ContainerFormat::FragmentedMp4, Box::new(out_file), &tracks)?;

    for (i, au) in access_units.iter().enumerate() {
        let is_keyframe = bitstream::nal_units(au).any(|nal| {
            let nal_type = nal.first().map_or(0, |b| b & 0x1F);
            nal_type == 5 // IDR slice
        });

        // 30 fps -> 10_000_000 / 30 ticks per frame
        let pts = Hns::from_ticks(i as i64 * 10_000_000 / 30);
        let duration = Hns::from_ticks(10_000_000 / 30);

        let packet = Packet {
            meta: PacketMeta { track: TrackId::Video, pts, duration, keyframe: is_keyframe },
            data: au.clone(),
        };

        writer.write_packet(packet.view())?;
    }

    writer.finish()?;
    println!("Successfully muxed to {}", output_path);
    Ok(())
}

//! play: minimal audio/video player. ffmpeg decodes (and plays audio through
//! PulseAudio/PipeWire); this program is the Wayland window (video) or the
//! terminal UI (audio).
//!
//! usage: play [-s] [-r] file|dir...
//! video keys: q/Esc quit | space pause | Left/Right -/+10s | Down/Up -/+60s | n/p next/prev
//! audio keys: q quit | space pause | arrows seek | n/p track | s shuffle | r repeat

mod audio;
mod common;
mod video;

use common::Playlist;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (mut shuffle, mut repeat) = (false, false);
    let mut i = 0;
    while i < args.len() && args[i].starts_with('-') && args[i].len() > 1 {
        match args[i].as_str() {
            "-s" => shuffle = true,
            "-r" => repeat = true,
            "--" => {
                i += 1;
                break;
            }
            _ => {
                eprintln!("usage: play [-s] [-r] file|dir...");
                std::process::exit(1);
            }
        }
        i += 1;
    }
    if i >= args.len() {
        eprintln!("usage: play [-s] [-r] file|dir...");
        std::process::exit(1);
    }
    let mut pl = Playlist::new(shuffle, repeat);
    for a in &args[i..] {
        pl.add_path(a);
    }
    if pl.files.is_empty() {
        eprintln!("play: no playable files");
        std::process::exit(1);
    }
    pl.build_order();
    let code = if common::audio_only(pl.cur_file()) { audio::run(pl) } else { video::run(pl) };
    std::process::exit(code);
}

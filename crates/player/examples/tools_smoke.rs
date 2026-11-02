//! Installs the YouTube components as the app would, then checks yt-dlp can use them.
//! Usage: tools_smoke

use app_core::{config::AppConfig, tools};
use app_player::Ytdlp;

fn main() {
    let started = std::time::Instant::now();
    let result = tools::install(|step| println!("{:>5.1}s {step:?}", started.elapsed().as_secs_f32()));
    println!("{:>5.1}s install: {result:?}", started.elapsed().as_secs_f32());
    println!("yt-dlp.exe: {} | deno.exe: {}", tools::ytdlp_exe().exists(), tools::deno_exe().exists());
    let ytdlp = Ytdlp::from_config(&AppConfig::default());
    println!("uses: {} | version: {:?} | solves challenges: {}", ytdlp.path, ytdlp.version(), ytdlp.solves_challenges());
}

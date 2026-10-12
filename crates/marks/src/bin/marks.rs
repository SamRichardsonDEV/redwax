//! `redwax-marks [--profile NAME] cookies|marks|index`, the eye on the browser
//! profile. Cookie values never reach the terminal, only the names.

use std::process::ExitCode;

use redwax_marks::{FirefoxProfile, ProfileCapability, read_cookies, read_marks};

const USAGE: &str = "usage: redwax-marks [--profile NAME] cookies|marks|index";

fn main() -> ExitCode {
    let mut profile_name: Option<String> = None;
    let mut subcommand: Option<String> = None;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--profile" => match arguments.next() {
                Some(name) => profile_name = Some(name),
                None => return fail("--profile needs a name"),
            },
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            _ if subcommand.is_none() => subcommand = Some(argument),
            _ => return fail(USAGE),
        }
    }

    let capability = match subcommand.as_deref() {
        Some("cookies") => ProfileCapability::Cookies,
        Some("marks" | "index") => ProfileCapability::Marks,
        _ => return fail(USAGE),
    };
    let firefox_profile = match FirefoxProfile::find_for(profile_name.as_deref(), capability) {
        Ok(firefox_profile) => firefox_profile,
        Err(error) => return fail(&error.to_string()),
    };
    println!("profile {}", firefox_profile.dir.display());

    match subcommand.as_deref() {
        Some("cookies") => match read_cookies(&firefox_profile) {
            Ok(cookies) => {
                println!("{} cookies", cookies.len());
                for cookie in &cookies {
                    println!("  {} {}", cookie.host, cookie.name);
                }
                ExitCode::SUCCESS
            }
            Err(error) => fail(&error.to_string()),
        },
        Some("marks") => match read_marks(&firefox_profile) {
            Ok(marks) => {
                println!(
                    "{} tracks, {} playlists, {} albums, {} stored locally",
                    marks.tracks.len(),
                    marks.playlists.len(),
                    marks.albums.len(),
                    marks.stored_locally.len()
                );
                for playlist_id in &marks.playlists {
                    println!("  playlist {playlist_id}");
                }
                for album_browse_id in &marks.albums {
                    println!("  album {album_browse_id}");
                }
                for video_id in &marks.tracks {
                    println!("  track {video_id}");
                }
                ExitCode::SUCCESS
            }
            Err(error) => fail(&error.to_string()),
        },
        Some("index") => match read_marks(&firefox_profile) {
            Ok(marks) => {
                println!("{} stored locally", marks.stored_locally.len());
                for video_id in &marks.stored_locally {
                    println!("  {video_id}");
                }
                ExitCode::SUCCESS
            }
            Err(error) => fail(&error.to_string()),
        },
        _ => unreachable!("the command was validated before profile discovery"),
    }
}

fn fail(message: &str) -> ExitCode {
    eprintln!("{message}");
    ExitCode::FAILURE
}

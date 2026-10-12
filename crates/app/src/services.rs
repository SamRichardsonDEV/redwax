use redwax_player::PlayerOptions;
use redwax_resolve::ResolverOptions;
use redwax_ytm::Session;

use crate::config::{self, Config, Quality};

/// Runtime and diagnostics must resolve with the same policy.
pub fn resolver_options(config: &Config, session: Option<&Session>) -> ResolverOptions {
    let identity = session.map(|session| session.identity().clone());
    ResolverOptions {
        cookies_path: Some(config.cookies_path.clone()),
        identity,
        quality: match config.quality {
            Quality::High => redwax_resolve::Quality::High,
            Quality::Fast => redwax_resolve::Quality::Fast,
        },
        library_dir: Some(config.library_dir.clone()),
        ytdlp_args: redwax_resolve::raw_options_to_args(&config.ytdl_raw_options),
        parallel: 3,
        cache_dir: config::cache_dir(),
    }
}

pub fn player_options(config: &Config) -> PlayerOptions {
    PlayerOptions {
        volume: config.volume,
        mpv_args: config.mpv_args.clone(),
        ytdl_raw_options: config.resolved_ytdl_raw_options(),
        prefetch_ahead: 3,
        ..PlayerOptions::default()
    }
}

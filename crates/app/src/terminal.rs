//! How album art reaches the screen. ratatui-image asks the terminal what it
//! supports, but inside tmux the answer comes from tmux, which claims Sixel
//! even when the real terminal cannot draw it and swallows the Kitty query.
//! Behind tmux the outer terminal decides, and Kitty graphics use passthrough.

use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Renderer {
    Kitty,
    Sixel,
    Halfblocks,
}

impl Renderer {
    pub fn from_config(renderer_name: &str) -> Option<Renderer> {
        match renderer_name {
            "tgp" | "kitty" => Some(Renderer::Kitty),
            "sixel" => Some(Renderer::Sixel),
            "halfcell" | "unicode" | "halfblocks" => Some(Renderer::Halfblocks),
            _ => None,
        }
    }

    pub fn draws_pixels(self) -> bool {
        matches!(self, Renderer::Kitty | Renderer::Sixel)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtSetup {
    detection: ArtDetection,
    pub in_tmux: bool,
    pub outer_terminal: Option<String>,
    pub reason: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArtDetection {
    Pending,
    QueryTerminal,
    Selected(Renderer),
}

impl ArtSetup {
    pub fn pending() -> ArtSetup {
        ArtSetup {
            detection: ArtDetection::Pending,
            in_tmux: false,
            outer_terminal: None,
            reason: "detection starts after the first frame",
        }
    }

    pub fn is_pending(&self) -> bool {
        self.detection == ArtDetection::Pending
    }

    pub fn renderer(&self) -> Option<Renderer> {
        match self.detection {
            ArtDetection::Selected(renderer) => Some(renderer),
            ArtDetection::Pending | ArtDetection::QueryTerminal => None,
        }
    }
}

const TGP_TERMINALS: [&str; 4] = ["ghostty", "kitty", "wezterm", "konsole"];

/// The config's choice, else Kitty when the outer terminal behind tmux is
/// known to draw it, else the terminal's own answer.
pub fn detect(renderer_preference: &str, in_tmux: Option<bool>, outer_terminal: Option<String>) -> ArtSetup {
    let in_tmux =
        in_tmux.unwrap_or_else(|| std::env::var_os("TMUX").is_some_and(|tmux_value| !tmux_value.is_empty()));
    let outer_terminal = outer_terminal.or_else(|| outer_terminal_name(in_tmux));
    choose(renderer_preference, in_tmux, outer_terminal)
}

pub fn choose(renderer_preference: &str, in_tmux: bool, outer_terminal: Option<String>) -> ArtSetup {
    if let Some(renderer) = Renderer::from_config(renderer_preference) {
        return ArtSetup {
            detection: ArtDetection::Selected(renderer),
            in_tmux,
            outer_terminal,
            reason: "set by art = ... in the config",
        };
    }
    if in_tmux && is_tgp_terminal(outer_terminal.as_deref()) {
        return ArtSetup {
            detection: ArtDetection::Selected(Renderer::Kitty),
            in_tmux,
            outer_terminal,
            reason: "Kitty graphics via tmux passthrough",
        };
    }
    ArtSetup {
        detection: ArtDetection::QueryTerminal,
        in_tmux,
        outer_terminal,
        reason: "reported by the terminal",
    }
}

/// Let graphics escape this pane to the real terminal. Pane scoped.
pub fn enable_tmux_passthrough() -> bool {
    if std::env::var_os("TMUX").is_none() {
        return false;
    }
    Command::new("tmux")
        .args(["set", "-p", "allow-passthrough", "on"])
        .output()
        .map(|command_output| command_output.status.success())
        .unwrap_or(false)
}

fn outer_terminal_name(in_tmux: bool) -> Option<String> {
    if in_tmux {
        let command_output = Command::new("tmux")
            .args(["display", "-p", "#{client_termname}"])
            .output()
            .ok()?;
        let terminal_name = String::from_utf8_lossy(&command_output.stdout).trim().to_string();
        return if terminal_name.is_empty() {
            None
        } else {
            Some(terminal_name)
        };
    }
    std::env::var("TERM_PROGRAM")
        .ok()
        .or_else(|| std::env::var("TERM").ok())
}

fn is_tgp_terminal(terminal_name: Option<&str>) -> bool {
    let terminal_name = terminal_name.unwrap_or("").to_lowercase();
    TGP_TERMINALS
        .iter()
        .any(|supported_terminal| terminal_name.contains(supported_terminal))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ghostty_behind_tmux_gets_kitty_graphics_despite_tmux_claiming_sixel() {
        let setup = detect("auto", Some(true), Some("xterm-ghostty".into()));
        assert_eq!(setup.renderer(), Some(Renderer::Kitty));
        assert!(setup.in_tmux);
    }

    #[test]
    fn test_config_preference_wins() {
        let setup = detect("sixel", Some(true), Some("xterm-ghostty".into()));
        assert_eq!(setup.renderer(), Some(Renderer::Sixel));
    }

    #[test]
    fn test_unknown_terminal_behind_tmux_falls_back_to_detection() {
        let setup = detect("auto", Some(true), Some("xterm-256color".into()));
        assert_eq!(setup.renderer(), None);
    }
}

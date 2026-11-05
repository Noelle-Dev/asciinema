//! Preset color themes for the metadata editor.
//!
//! All but Campbell and xterm are the themes bundled with asciinema-player
//! (assets/asciinema-player.css); bright colors the player leaves undefined
//! fall back to the normal ones, as the player does. Campbell is Windows
//! Terminal's default scheme; xterm is xterm's default palette.

pub struct Preset {
    pub name: &'static str,
    pub fg: &'static str,
    pub bg: &'static str,
    pub palette: [&'static str; 16],
}

pub const PRESETS: &[Preset] = &[
    Preset {
        name: "Campbell",
        fg: "#cccccc",
        bg: "#0c0c0c",
        palette: [
            "#0c0c0c", "#c50f1f", "#13a10e", "#c19c00", "#0037da", "#881798", "#3a96dd", "#cccccc",
            "#767676", "#e74856", "#16c60c", "#f9f1a5", "#3b78ff", "#b4009e", "#61d6d6", "#f2f2f2",
        ],
    },
    Preset {
        name: "asciinema",
        fg: "#cccccc",
        bg: "#121314",
        palette: [
            "#000000", "#dd3c69", "#4ebf22", "#ddaf3c", "#26b0d7", "#b954e1", "#54e1b9", "#d9d9d9",
            "#4d4d4d", "#dd3c69", "#4ebf22", "#ddaf3c", "#26b0d7", "#b954e1", "#54e1b9", "#ffffff",
        ],
    },
    Preset {
        name: "Dracula",
        fg: "#f8f8f2",
        bg: "#282a36",
        palette: [
            "#21222c", "#ff5555", "#50fa7b", "#f1fa8c", "#bd93f9", "#ff79c6", "#8be9fd", "#f8f8f2",
            "#6272a4", "#ff6e6e", "#69ff94", "#ffffa5", "#d6acff", "#ff92df", "#a4ffff", "#ffffff",
        ],
    },
    Preset {
        name: "Gruvbox Dark",
        fg: "#fbf1c7",
        bg: "#282828",
        palette: [
            "#282828", "#cc241d", "#98971a", "#d79921", "#458588", "#b16286", "#689d6a", "#a89984",
            "#7c6f65", "#fb4934", "#b8bb26", "#fabd2f", "#83a598", "#d3869b", "#8ec07c", "#fbf1c7",
        ],
    },
    Preset {
        name: "Monokai",
        fg: "#f8f8f2",
        bg: "#272822",
        palette: [
            "#272822", "#f92672", "#a6e22e", "#f4bf75", "#66d9ef", "#ae81ff", "#a1efe4", "#f8f8f2",
            "#75715e", "#f92672", "#a6e22e", "#f4bf75", "#66d9ef", "#ae81ff", "#a1efe4", "#f9f8f5",
        ],
    },
    Preset {
        name: "Nord",
        fg: "#eceff4",
        bg: "#2e3440",
        palette: [
            "#3b4252", "#bf616a", "#a3be8c", "#ebcb8b", "#81a1c1", "#b48ead", "#88c0d0", "#eceff4",
            "#3b4252", "#bf616a", "#a3be8c", "#ebcb8b", "#81a1c1", "#b48ead", "#88c0d0", "#eceff4",
        ],
    },
    Preset {
        name: "Seti",
        fg: "#cacecd",
        bg: "#111213",
        palette: [
            "#323232", "#c22832", "#8ec43d", "#e0c64f", "#43a5d5", "#8b57b5", "#8ec43d", "#eeeeee",
            "#323232", "#c22832", "#8ec43d", "#e0c64f", "#43a5d5", "#8b57b5", "#8ec43d", "#ffffff",
        ],
    },
    Preset {
        name: "Solarized Dark",
        fg: "#839496",
        bg: "#002b36",
        palette: [
            "#073642", "#dc322f", "#859900", "#b58900", "#268bd2", "#d33682", "#2aa198", "#eee8d5",
            "#002b36", "#cb4b16", "#586e75", "#657b83", "#839496", "#6c71c4", "#93a1a1", "#fdf6e3",
        ],
    },
    Preset {
        name: "Solarized Light",
        fg: "#657b83",
        bg: "#fdf6e3",
        palette: [
            "#073642", "#dc322f", "#859900", "#b58900", "#268bd2", "#d33682", "#2aa198", "#eee8d5",
            "#002b36", "#cb4b16", "#586e75", "#657c83", "#839496", "#6c71c4", "#93a1a1", "#fdf6e3",
        ],
    },
    Preset {
        name: "Tango",
        fg: "#cccccc",
        bg: "#121314",
        palette: [
            "#000000", "#cc0000", "#4e9a06", "#c4a000", "#3465a4", "#75507b", "#06989a", "#d3d7cf",
            "#555753", "#ef2929", "#8ae234", "#fce94f", "#729fcf", "#ad7fa8", "#34e2e2", "#eeeeec",
        ],
    },
    Preset {
        name: "xterm",
        fg: "#e5e5e5",
        bg: "#000000",
        palette: [
            "#000000", "#cd0000", "#00cd00", "#cdcd00", "#0000ee", "#cd00cd", "#00cdcd", "#e5e5e5",
            "#7f7f7f", "#ff0000", "#00ff00", "#ffff00", "#5c5cff", "#ff00ff", "#00ffff", "#ffffff",
        ],
    },
];

impl Preset {
    pub fn theme(&self) -> crate::tty::TtyTheme {
        let parse = |s: &str| super::edits::parse_hex(s).expect("valid preset color");

        crate::tty::TtyTheme {
            fg: parse(self.fg),
            bg: parse(self.bg),
            palette: self.palette.iter().map(|c| parse(c)).collect(),
        }
    }
}

/// Name of the preset `theme` matches exactly, if any.
pub fn matching(theme: &crate::tty::TtyTheme) -> Option<&'static str> {
    PRESETS
        .iter()
        .find(|p| {
            let t = p.theme();
            t.fg == theme.fg && t.bg == theme.bg && t.palette == theme.palette
        })
        .map(|p| p.name)
}

#[cfg(test)]
mod tests {
    use super::PRESETS;

    #[test]
    fn presets_are_valid() {
        for p in PRESETS {
            let t = p.theme();
            assert_eq!(t.palette.len(), 16, "{}", p.name);
        }
    }
}

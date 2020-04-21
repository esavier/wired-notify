#![allow(dead_code)]

use std::{
    sync::mpsc::{self, Receiver},
    time::Duration,
    env,
    io,
    path::PathBuf,
    fmt::{self, Display, Formatter},
};

use serde::Deserialize;
use notify::{RecommendedWatcher, Watcher, RecursiveMode, DebouncedEvent};

use crate::{
    maths_utility::{Vec2, Rect, MinMax},
    rendering::layout::{LayoutBlock, LayoutElement},
    bus::dbus::Notification,
};

static mut CONFIG: Option<Config> = None;

#[derive(Debug)]
pub enum Error {
    // Config file not found.
    NotFound,
    // Validation error.
    Validate(&'static str),
    // IO error reading file.
    Io(io::Error),
    // Deserialization error.
    Ron(ron::de::Error),
    // Watch error.
    Watch(notify::Error),
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::NotFound => None,
            Error::Validate(_) => None,
            Error::Io(err) => err.source(),
            Error::Ron(err) => err.source(),
            Error::Watch(err) => err.source(),
        }
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotFound => write!(f, "No config found"),
            Error::Validate(problem) => write!(f, "Error validating config file: {}", problem), 
            Error::Io(err) => write!(f, "Error reading config file: {}", err), 
            Error::Ron(err) => write!(f, "Problem with config file: {}", err), 
            Error::Watch(err) => write!(f, "Error watching config directory: {}", err), 
        }
    }
}

pub struct ConfigWatcher {
    watcher: RecommendedWatcher,
    pub receiver: Receiver<DebouncedEvent>,
}

#[derive(Debug, Deserialize)]
pub struct Config {
    pub max_notifications: usize,
    // Minimum window width and height.  This is used to create the base rect that the notification
    // grows within.
    // The notification window will never be smaller than this.
    // A value of 1 means that the window will generally always resize with notification, unless
    // you have a 1x1 pixel notification...
    pub min_window_width: u32,
    pub min_window_height: u32,

    pub timeout: i32,           // Default timeout.
    pub poll_interval: u64,

    // Draws rectangles around elements.
    pub debug: bool,
    pub debug_color: Color,

    pub shortcuts: ShortcutsConfig,

    pub layout: LayoutBlock,
}

impl Config {
    // Initialize the config.  This does a two things:
    // - Attempts to locate and load a config file on the machine, and if it can't, then loads the
    // default config.
    // - If config was loaded successfully, then sets up a watcher on the config file to watch for changes,
    // and returns the watcher or None.
    pub fn init() -> Option<ConfigWatcher> {
        unsafe {
            assert!(CONFIG.is_none());
            let cfg_file = Config::installed_config();
            match cfg_file {
                Some(f) => {
                    let cfg = Config::load(f.clone());
                    match cfg {
                        Ok(c) => CONFIG = Some(c),
                        Err(e) => {
                            println!("Found a config but couldn't load it, so will use default one for now:\n\t{}", e);
                            CONFIG = Some(Config::default());
                        }
                    }

                    // Watch the config file for changes, even if it didn't load correctly; we
                    // assume that the config we found is the one we're using.
                    // It would be nice to be able to watch the config directories for when a user
                    // creates a config, but it seems impractical to watch that many directories.
                    let watch = Config::watch(f);
                    match watch {
                        Ok(w) => return Some(w),
                        Err(e) => {
                            println!("There was a problem watching the config for changes; so won't watch:\n\t{}", e);
                            return None;
                        },
                    }
                }

                None => {
                    println!("Couldn't load a config because we couldn't find one, so will use default.");
                    CONFIG = Some(Config::default());
                    return None;
                },
            };

        }
    }

    // Get immutable reference to global config variable.
    pub fn get() -> &'static Config {
        unsafe {
            assert!(CONFIG.is_some());
            // TODO: can as_ref be removed?
            CONFIG.as_ref().unwrap()
        }
    }

    // Get mutable refernce to global config variable.
    pub fn get_mut() -> &'static mut Config {
        unsafe {
            assert!(CONFIG.is_some());
            // TODO: can as_ref be removed?
            CONFIG.as_mut().unwrap()
        }
    }

    // Attempt to load the config again.
    // If we can, then replace the existing config.
    // If we can't, then do nothing.
    pub fn try_reload(path: PathBuf) {
        match Config::load(path) {
            Ok(cfg) => unsafe { CONFIG = Some(cfg) },
            Err(e) => println!("Tried to reload the config but couldn't: {}", e),
        }
    }

    // https://github.com/alacritty/alacritty/blob/f14d24542c3ceda3b508c707eb79cf2fe2a04bd1/alacritty/src/config/mod.rs#L98
    fn installed_config() -> Option<PathBuf> {
        xdg::BaseDirectories::with_prefix("wired")
            .ok()
            .and_then(|xdg| xdg.find_config_file("wired.ron"))
            .or_else(|| {
                xdg::BaseDirectories::new()
                    .ok()
                    .and_then(|fallback| fallback.find_config_file("wired.ron"))
            })
            .or_else(|| {
                if let Ok(home) = env::var("HOME") {
                    // Fallback path: `$HOME/.config/wired/wired.ron`
                    let fallback = PathBuf::from(&home).join(".config/wired/wired.ron");
                    if fallback.exists() {
                        return Some(fallback);
                    }

                    // Fallback path: `$HOME/.wired.ron`
                    let fallback = PathBuf::from(&home).join(".wired.ron");
                    if fallback.exists() {
                        return Some(fallback);
                    }
                }

                None
            })
    }

    // Load config or return error.
    pub fn load(path: PathBuf) -> Result<Self, Error> {
        let cfg_string = std::fs::read_to_string(path);
        let cfg_string = match cfg_string {
            Ok(string) => string,
            Err(e) => return Err(Error::Io(e)),
        };

        let config: Result<Self, _> = ron::de::from_str(cfg_string.as_str());
        match config {
            Ok(cfg) => return cfg.validate(),
            Err(e) => return Err(Error::Ron(e)),
        };
    }

    // Watch config file for changes, and send message to `Configwatcher` when something
    // happens.
    pub fn watch(mut path: PathBuf) -> Result<ConfigWatcher, Error> {
        let (sender, receiver) = mpsc::channel();

        // Duration is a debouncing period.
        let mut watcher = notify::watcher(sender, Duration::from_millis(10))
            .expect("Unable to spawn file watcher.");

        // Watch dir.
        path.pop();
        let path = std::fs::canonicalize(path).expect("Couldn't canonicalize path, wtf.");
        let result = watcher.watch(path, RecursiveMode::NonRecursive);
        match result {
            Ok(_) => return Ok(ConfigWatcher { watcher, receiver }),
            Err(e) => return Err(Error::Watch(e)),
        };
    }

    // Verify that the config is constructed correctly.
    fn validate(self) -> Result<Self, Error> {
        match &self.layout.params {
            LayoutElement::NotificationBlock(_) => Ok(self),
            _ => Err(Error::Validate("The first LayoutBlock params must be of type NotificationBlock!")),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        let cfg_string = include_str!("../wired.ron");
        ron::de::from_str(cfg_string)
            .expect("Failed to parse default config.  Something is fucked up.\n")
    }
}

#[derive(Debug, Deserialize)]
pub struct ShortcutsConfig {
    pub notification_close: u8,
    pub notification_closeall: u8,
    pub notification_pause: u8,
    pub notification_url: u8,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Padding {
    pub left: f64,
    pub right: f64,
    pub top: f64,
    pub bottom: f64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Offset {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Deserialize, Clone)]
pub enum AnchorPosition {
    ML,
    TL,
    MT,
    TR,
    MR,
    BR,
    MB,
    BL,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Color {
    pub r: f64,
    pub g: f64,
    pub b: f64,
    pub a: f64,
}

#[derive(Default, Debug, Deserialize, Clone)]
pub struct TextDimensions {
    pub width: MinMax,
    pub height: MinMax,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TextDimensionVariants {
    pub dimensions: TextDimensions,
    pub dimensions_image_hint: TextDimensions,
    pub dimensions_image_app: TextDimensions,
    pub dimensions_image_both: TextDimensions,
}

impl Color {
    pub fn new(r: f64, g: f64, b: f64, a: f64) -> Self {
        Color { r, g, b, a }
    }
}

impl Padding {
    pub fn new(left: f64, right: f64, top: f64, bottom: f64) -> Self {
        Padding { left, right, top, bottom }
    }

    pub fn width(&self) -> f64 {
        self.left + self.right
    }
    pub fn height(&self) -> f64 {
        self.top + self.bottom
    }
}

impl AnchorPosition {
    pub fn get_pos(&self, rect: &Rect) -> Vec2 {
        match self {
            AnchorPosition::ML => rect.mid_left(),
            AnchorPosition::TL => rect.top_left(),
            AnchorPosition::MT => rect.mid_top(),
            AnchorPosition::TR => rect.top_right(),
            AnchorPosition::MR => rect.mid_right(),
            AnchorPosition::BR => rect.bottom_right(),
            AnchorPosition::MB => rect.mid_bottom(),
            AnchorPosition::BL => rect.bottom_left(),
        }
    }
}

impl TextDimensionVariants {
    pub fn get_dimensions(&self, notification: &Notification) -> &TextDimensions {
        match (notification.app_image.is_some(), notification.hint_image.is_some()) {
            (true, true) => &self.dimensions_image_both,
            (true, false) => &self.dimensions_image_app,
            (false, true) => &self.dimensions_image_hint,
            (false, false) => &self.dimensions,
        }
    }
}


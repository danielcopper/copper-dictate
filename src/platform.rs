pub(crate) mod clipboard;
pub(crate) mod data_control;
pub(crate) mod kde;
pub(crate) mod keyboard;
pub(crate) mod remote_desktop;

use std::fmt::Display;

use crate::config::InsertionConfig;
use crate::log;
use crate::session::Insertion;
use crate::xdg::Env;

/// Where the desktop offers no way to insert: the final text reaches only `toggle`'s output.
pub(crate) struct NoInsertion;

impl Insertion for NoInsertion {
    fn insert(&self, _text: String) {}
}

/// The insertion this desktop allows; where there is none, it says why once and inserts nowhere.
pub(crate) fn insertion(env: Env<'_>, config: InsertionConfig) -> Box<dyn Insertion> {
    if env("WAYLAND_DISPLAY").is_none_or(|display| display.is_empty()) {
        cannot_insert("no Wayland session (WAYLAND_DISPLAY is not set)");
        return Box::new(NoInsertion);
    }
    match kde::start(config, remote_desktop::token_path(env)) {
        Ok(paste) => Box::new(paste),
        Err(error) => {
            cannot_insert(format_args!("the insertion does not start: {error}"));
            Box::new(NoInsertion)
        }
    }
}

pub(crate) fn cannot_insert(reason: impl Display) {
    log::eprint(format_args!(
        "cannot insert at the cursor: {reason}; the final text reaches only toggle's output"
    ));
}

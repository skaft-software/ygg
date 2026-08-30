#![allow(missing_docs)]

pub mod composer;
pub mod composer_surface;
pub(crate) mod context;
pub(crate) mod fuzzy;
pub mod keymap;
pub(crate) mod layout;
pub mod pickers;
pub(crate) mod splash;
pub mod terminal;
pub mod theme;
#[cfg(test)]
mod theme_pack;
mod theme_schema;
pub mod view;

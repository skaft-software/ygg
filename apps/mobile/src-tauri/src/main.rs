//! Desktop development entry point for the native companion shell.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    ygg_mobile_lib::run();
}

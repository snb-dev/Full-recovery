// No console window on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    fullrecovery_desktop_lib::run();
}

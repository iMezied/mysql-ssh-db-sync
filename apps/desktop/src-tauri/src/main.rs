// Windows release builds must not pop a console window behind the GUI.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    db_sync_desktop::run();
}

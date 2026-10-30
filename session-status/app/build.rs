fn main() {
    // Headless builds (--no-default-features) have no Tauri app to generate a context for.
    #[cfg(feature = "gui")]
    tauri_build::build();
}

fn main() {
    let manifest = tauri_build::AppManifest::new().commands(&[
        "daemon_status",
        "devices_snapshot",
        "discover_devices",
        "set_listener",
        "set_discovery",
        "rename_device",
        "set_device_address",
        "remove_device",
        "send_clipboard",
        "pair_invite",
        "pair_import",
        "pair_status",
        "pair_confirm",
        "pair_cancel",
    ]);
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(manifest))
        .expect("build Lyteboard desktop permissions");
}

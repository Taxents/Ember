fn main() {
    slint_build::compile("ui/app.slint").expect("slint");

    // The exe's own icon (Explorer, shortcuts, the taskbar before the window opens) and name
    // (Task Manager shows the description, which would otherwise be the crate's name).
    println!("cargo:rerun-if-changed=icons/ember.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new()
            .set_icon("icons/ember.ico")
            .set("FileDescription", "Ember")
            .set("ProductName", "Ember")
            .set("OriginalFilename", "ember.exe")
            .compile()
            .expect("icon resource");
    }

    // Our fanart.tv project key, from the gitignored secrets.toml (`fanart_key = "…"`). Without
    // it the app simply shows no artist logos.
    println!("cargo:rerun-if-changed=../secrets.toml");
    let key = std::fs::read_to_string("../secrets.toml")
        .unwrap_or_default()
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once('=')?;
            (k.trim() == "fanart_key").then(|| v.trim().trim_matches('"').to_string())
        })
        .unwrap_or_default();
    println!("cargo:rustc-env=FANART_KEY={key}");
}

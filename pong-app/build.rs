//! On Windows: the icon and version information in the executable itself.
//! The icon is the window's, the taskbar's and the tray's (they all take
//! the executable's first icon resource).

fn main() {
    println!("cargo:rerun-if-changed=assets/Pong.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/Pong.ico");
    res.set("ProductName", "Pong");
    res.set("FileDescription", "Pong");
    if let Err(e) = res.compile() {
        println!("cargo:warning=Pong's window without its icon: {e}");
    }
}

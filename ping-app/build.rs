//! On Windows: the icon and version information in Ping.exe itself.

fn main() {
    println!("cargo:rerun-if-changed=assets/Ping.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/Ping.ico");
    res.set("ProductName", "Ping");
    res.set("FileDescription", "Ping");
    if let Err(e) = res.compile() {
        println!("cargo:warning=Ping.exe without its icon: {e}");
    }
}

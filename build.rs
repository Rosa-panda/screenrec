fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("ProductName", "screenrec");
        res.set("FileDescription", "screenrec 录屏工具");
        res.set("CompanyName", "Rosa-panda");
        res.set("LegalCopyright", "https://github.com/Rosa-panda/screenrec");
        if let Err(e) = res.compile() {
            println!("cargo:warning=嵌入图标失败: {e}");
        }
    }
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=build.rs");
}

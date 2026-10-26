use shadow_rs::ShadowBuilder;

fn main() {
    // The command dispatcher exceeds the default Windows stack in debug builds.
    // Reserve enough stack for CLI parsing and schema generation on both toolchains.
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.ends_with("windows-msvc") {
        println!("cargo:rustc-link-arg-bin=komorebic=/STACK:8388608");
    } else if target.ends_with("windows-gnu") {
        println!("cargo:rustc-link-arg-bin=komorebic=-Wl,--stack,8388608");
    }

    if std::fs::metadata("applications.json").is_err() {
        let applications_json = reqwest::blocking::get(
        "https://raw.githubusercontent.com/LGUG2Z/komorebi-application-specific-configuration/master/applications.json"
    ).unwrap().text().unwrap();
        std::fs::write("applications.json", applications_json).unwrap();
    }

    ShadowBuilder::builder().build().unwrap();
}

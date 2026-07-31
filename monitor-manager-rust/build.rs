use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    if env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }

    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("monitor-manager.rc");
    let version = env::var("CARGO_PKG_VERSION").unwrap();
    let mut parts = version
        .split('.')
        .map(|part| part.parse::<u16>().unwrap_or(0));
    let major = parts.next().unwrap_or(0);
    let minor = parts.next().unwrap_or(0);
    let patch = parts.next().unwrap_or(0);
    let icon = manifest_dir
        .join("icon.ico")
        .display()
        .to_string()
        .replace('\\', "/");
    let manifest_template = manifest_dir.join("app.manifest");
    let manifest_contents = fs::read_to_string(&manifest_template).unwrap().replace(
        "version=\"0.0.0.0\"",
        &format!("version=\"{major}.{minor}.{patch}.0\""),
    );
    let generated_manifest =
        PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("monitor-manager.manifest");
    fs::write(&generated_manifest, manifest_contents).unwrap();
    let app_manifest = generated_manifest.display().to_string().replace('\\', "/");

    let resource = format!(
        r#"1 ICON "{icon}"
1 24 "{app_manifest}"

1 VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x40004L
FILETYPE 0x1L
FILESUBTYPE 0x0L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904B0"
        BEGIN
            VALUE "CompanyName", "Demoen\0"
            VALUE "FileDescription", "Monitor Manager\0"
            VALUE "FileVersion", "{version}\0"
            VALUE "InternalName", "monitor-manager\0"
            VALUE "LegalCopyright", "MIT License\0"
            VALUE "OriginalFilename", "monitor-manager.exe\0"
            VALUE "ProductName", "Monitor Manager\0"
            VALUE "ProductVersion", "{version}\0"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x0409, 1200
    END
END
"#
    );

    fs::write(&output, resource).unwrap();
    embed_resource::compile(output, embed_resource::NONE);
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-changed=icon.ico");
}

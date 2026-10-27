use color_eyre::eyre;
use winreg::RegKey;
use winreg::enums::HKEY_LOCAL_MACHINE;

pub const MIN_BUILD: u32 = 26100;
pub const MIN_UBR: u32 = 2605;

const CURRENT_VERSION_KEY: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";

#[must_use]
pub fn is_supported_build(build: u32, ubr: u32) -> bool {
    build > MIN_BUILD || (build == MIN_BUILD && ubr >= MIN_UBR)
}

pub fn require_supported_windows() -> eyre::Result<()> {
    match read_build() {
        Ok((build, ubr)) => require_build(build, ubr),
        Err(error) => eyre::bail!(
            "komorebi requires Windows 11 24H2 or newer (minimum build {MIN_BUILD}.{MIN_UBR}); could not detect the current build: {error}"
        ),
    }
}

pub(crate) fn require_build(build: u32, ubr: u32) -> eyre::Result<()> {
    if is_supported_build(build, ubr) {
        return Ok(());
    }
    eyre::bail!(
        "komorebi requires Windows 11 24H2 or newer (minimum build {MIN_BUILD}.{MIN_UBR}); detected build {build}.{ubr}"
    )
}

fn read_build() -> eyre::Result<(u32, u32)> {
    let current_version = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey(CURRENT_VERSION_KEY)?;
    let build: String = current_version.get_value("CurrentBuildNumber")?;
    let ubr: u32 = current_version.get_value("UBR")?;

    Ok((build.parse()?, ubr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_floor_is_26100_2605() {
        assert!(is_supported_build(26100, 2605));
        assert!(!is_supported_build(26100, 2604));
        assert!(!is_supported_build(26099, 9999));
        assert!(is_supported_build(26200, 0));
        let error = require_build(26100, 2604).unwrap_err().to_string();
        assert!(error.contains("26100.2605"));
        assert!(error.contains("26100.2604"));
        assert!(require_build(26100, 2605).is_ok());
    }
}

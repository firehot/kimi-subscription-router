//! 敏感文件与目录的跨平台私有权限。

use std::io;
use std::path::Path;

#[cfg(unix)]
fn restrict(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(windows)]
fn restrict(path: &Path, directory: bool) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows_sys::Win32::Security::{
        SetFileSecurityW, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR,
    };

    // OW 是对象所有者，SY 是本地 SYSTEM。受保护 DACL 不继承父目录的宽松权限。
    let sddl = if directory {
        "D:P(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)"
    } else {
        "D:P(A;;FA;;;OW)(A;;FA;;;SY)"
    };
    let descriptor_text = sddl.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            descriptor_text.as_ptr(),
            1,
            &mut descriptor,
            ptr::null_mut(),
        )
    };
    if converted == 0 {
        return Err(io::Error::last_os_error());
    }

    let path = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let applied = unsafe {
        SetFileSecurityW(
            path.as_ptr(),
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            descriptor,
        )
    };
    unsafe {
        LocalFree(descriptor.cast());
    }
    if applied == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn restrict(_path: &Path, _directory: bool) -> io::Result<()> {
    Ok(())
}

/// 将文件限制为仅当前所有者与系统服务可访问。
pub fn restrict_file(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    return restrict(path, 0o600);
    #[cfg(not(unix))]
    return restrict(path, false);
}

/// 将目录及其后续子项限制为仅当前所有者与系统服务可访问。
pub fn restrict_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    return restrict(path, 0o700);
    #[cfg(not(unix))]
    return restrict(path, true);
}

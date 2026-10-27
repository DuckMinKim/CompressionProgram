//! Windows 파일 연결 — `.bzx` 파일에 아이콘을 붙이고, 더블클릭하면 Pouch로 열리게 한다.
//! 잠시 쓰던 `.pouch` 확장자도 같이 연결한다.
//!
//! 관리자 권한 없이 현재 사용자 영역(HKEY_CURRENT_USER\Software\Classes)에만 쓴다.
//!
//! ```text
//! HKCU\Software\Classes\.bzx                           (기본값) = "pouch.archive"   (.pouch도 같음)
//! HKCU\Software\Classes\pouch.archive                (기본값) = "Pouch 압축 파일"
//! HKCU\Software\Classes\pouch.archive\DefaultIcon    (기본값) = "...\pouch\pouch.ico",0
//! HKCU\Software\Classes\pouch.archive\shell\open\command (기본값) = "...\pouch-gui.exe" "%1"
//! ```

/// 아이콘 파일 (여러 크기가 든 .ico) — 실행 파일 안에 들어 있다가 등록할 때 풀어 놓는다
pub const ICON_ICO: &[u8] = include_bytes!("../assets/pouch.ico");
/// 창 아이콘용 256px PNG
pub const ICON_PNG: &[u8] = include_bytes!("../assets/pouch-256.png");

#[cfg(windows)]
pub use imp::*;

/// 이 프로그램이 관리자 권한으로 실행 중인지.
/// 관리자 권한 창에는 윈도우가 일반 탐색기에서 끌어다 놓기를 막는다 (UIPI 보안 정책).
pub fn is_elevated() -> bool {
    #[cfg(windows)]
    {
        imp::process_is_elevated()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[cfg(windows)]
mod imp {
    use super::ICON_ICO;
    use anyhow::{Context, Result};
    use std::path::{Path, PathBuf};
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    const PROG_ID: &str = "pouch.archive";
    /// 예전 이름으로 등록했던 것 (등록·해제할 때 정리)
    const LEGACY_PROG_ID: &str = "bzx.archive";
    /// 연결할 확장자 (새 이름 + 예전 이름)
    const EXTENSIONS: [&str; 2] = [".bzx", ".pouch"];
    const CLASSES: &str = r"Software\Classes";

    /// `.bzx` 파일을 `exe`(보통 pouch-gui.exe)와 연결하고 아이콘을 등록한다.
    pub fn register(exe: &Path) -> Result<()> {
        // 아이콘을 %LOCALAPPDATA%\pouch\pouch.ico 로 풀어 둔다 (exe를 옮겨도 아이콘은 유지)
        let icon_path = icon_dir()?.join("pouch.ico");
        std::fs::write(&icon_path, ICON_ICO)
            .with_context(|| format!("아이콘 파일을 쓸 수 없음: {}", icon_path.display()))?;

        let classes = RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey_with_flags(CLASSES, winreg::enums::KEY_ALL_ACCESS)?;

        // .bzx와 잠시 쓰던 .pouch 모두 Pouch로 연다
        for e in EXTENSIONS {
            let (ext, _) = classes.create_subkey(e)?;
            ext.set_value("", &PROG_ID)?;
        }
        // 예전 이름(bzx)으로 등록했던 것은 정리한다
        let _ = classes.delete_subkey_all(LEGACY_PROG_ID);

        let (prog, _) = classes.create_subkey(PROG_ID)?;
        prog.set_value("", &"Pouch 압축 파일")?;
        let (icon, _) = prog.create_subkey("DefaultIcon")?;
        icon.set_value("", &format!("\"{}\",0", icon_path.display()))?;
        let (cmd, _) = prog.create_subkey(r"shell\open\command")?;
        cmd.set_value("", &format!("\"{}\" \"%1\"", exe.display()))?;

        // 예전에 "연결 프로그램 → pouch-gui.exe"로 직접 고른 경우, 윈도우는 이 키를 본다
        if let Some(exe_name) = exe.file_name() {
            let (app, _) = classes.create_subkey(Path::new("Applications").join(exe_name))?;
            let (app_icon, _) = app.create_subkey("DefaultIcon")?;
            app_icon.set_value("", &format!("\"{}\",0", icon_path.display()))?;
        }

        notify_shell();
        Ok(())
    }

    /// 연결을 지운다 (우리가 만든 것만)
    pub fn unregister() -> Result<()> {
        let classes = RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey_with_flags(CLASSES, winreg::enums::KEY_ALL_ACCESS)?;
        let _ = classes.delete_subkey_all(PROG_ID);
        if let Some(exe_name) = std::env::current_exe()
            .ok()
            .and_then(|e| e.file_name().map(|n| n.to_owned()))
        {
            let _ = classes
                .delete_subkey_all(Path::new("Applications").join(exe_name).join("DefaultIcon"));
        }
        let _ = classes.delete_subkey_all(LEGACY_PROG_ID);
        // 확장자는 우리 것을 가리킬 때만 지운다 (다른 프로그램이 가져갔으면 그대로 둠)
        for e in EXTENSIONS {
            if let Ok(ext) = classes.open_subkey(e) {
                let current: String = ext.get_value("").unwrap_or_default();
                if current == PROG_ID || current == LEGACY_PROG_ID {
                    drop(ext);
                    let _ = classes.delete_subkey_all(e);
                }
            }
        }
        if let Ok(dir) = icon_dir() {
            let _ = std::fs::remove_file(dir.join("pouch.ico"));
        }
        notify_shell();
        Ok(())
    }

    /// 지금 `.bzx`가 이 `exe`와 연결되어 있는지
    pub fn is_registered(exe: &Path) -> bool {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let Ok(ext) = hkcu.open_subkey(format!(r"{CLASSES}\.bzx")) else {
            return false;
        };
        let prog: String = ext.get_value("").unwrap_or_default();
        if prog != PROG_ID {
            return false;
        }
        let Ok(cmd) = hkcu.open_subkey(format!(r"{CLASSES}\{PROG_ID}\shell\open\command")) else {
            return false;
        };
        let cmd: String = cmd.get_value("").unwrap_or_default();
        cmd.to_lowercase()
            .contains(&exe.display().to_string().to_lowercase())
    }

    pub(super) fn process_is_elevated() -> bool {
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
        use windows_sys::Win32::Security::{
            GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
        };
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        // SAFETY: 현재 프로세스의 토큰을 읽기 전용으로 열고, 크기가 맞는 구조체에 정보를 받은 뒤 닫는다
        unsafe {
            let mut token: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return false;
            }
            let mut elevation = TOKEN_ELEVATION::default();
            let mut len = 0u32;
            let ok = GetTokenInformation(
                token,
                TokenElevation,
                (&mut elevation as *mut TOKEN_ELEVATION).cast(),
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut len,
            );
            CloseHandle(token);
            ok != 0 && elevation.TokenIsElevated != 0
        }
    }

    fn icon_dir() -> Result<PathBuf> {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .context("LOCALAPPDATA 폴더를 찾을 수 없습니다")?;
        let dir = base.join("pouch");
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    /// 탐색기에 "파일 연결이 바뀌었다"고 알려서 아이콘을 바로 새로 그리게 한다
    fn notify_shell() {
        use windows_sys::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify};
        // SAFETY: 포인터 인자 없이 알림만 보내는 호출
        unsafe {
            SHChangeNotify(
                SHCNE_ASSOCCHANGED as i32,
                SHCNF_IDLIST,
                std::ptr::null(),
                std::ptr::null(),
            )
        };
    }
}

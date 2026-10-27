//! 빌드 스크립트: Windows에서 실행 파일(.exe)에 아이콘을 넣는다.

fn main() {
    println!("cargo:rerun-if-changed=assets/bzx.ico");
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/bzx.ico");
        res.set("FileDescription", "bzx archiver"); // rc.exe 인코딩 문제를 피하려고 영어로
        res.set("ProductName", "bzx");
        // 리소스 컴파일러(rc.exe)가 없어도 빌드는 계속되게 한다 — 아이콘만 빠짐
        if let Err(e) = res.compile() {
            println!(
                "cargo:warning=exe에 아이콘을 넣지 못했습니다 (Windows SDK의 rc.exe 필요): {e}"
            );
        }
    }
}

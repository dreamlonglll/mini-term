use super::*;

#[test]
fn 本地预览读取只接受限额内普通文件() {
    let dir = std::env::temp_dir().join(format!("mt-preview-http-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);

    let small = dir.join("small.png");
    std::fs::write(&small, b"small-image").unwrap();
    assert_eq!(
        fetch_local_preview_bytes(&small).unwrap().as_slice(),
        b"small-image"
    );
    assert!(
        fetch_local_preview_bytes(&dir).is_err(),
        "目录不得作为预览资源读取"
    );

    let oversized = dir.join("oversized.png");
    let file = std::fs::File::create(&oversized).unwrap();
    file.set_len(PREVIEW_IMAGE_MAX_BYTES + 1).unwrap();
    drop(file);
    assert!(
        fetch_local_preview_bytes(&oversized).is_err(),
        "超过硬上限的稀疏文件必须在读取前拒绝"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn 远程预览异步拉取验证成功_非2xx拒绝与超限拒绝() {
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;

    fn serve_once(response: &'static [u8]) -> (u16, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut req = [0u8; 512];
                let _ = stream.read(&mut req);
                let _ = stream.write_all(response);
            }
        });
        (port, handle)
    }

    // 1. 正常 200 返回字节
    let (p1, h1) =
        serve_once(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nPNG!");
    let ok =
        futures::executor::block_on(fetch_remote_bytes(format!("http://127.0.0.1:{p1}/a.png")))
            .expect("正常响应应成功返回");
    assert_eq!(ok, b"PNG!");
    h1.join().unwrap();

    // 2. 404 拒绝
    let (p2, h2) =
        serve_once(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    assert!(
        futures::executor::block_on(fetch_remote_bytes(format!("http://127.0.0.1:{p2}/missing")))
            .is_err()
    );
    h2.join().unwrap();

    // 3. Content-Length 超过 32MB 直接拒绝
    let (p3, h3) =
        serve_once(b"HTTP/1.1 200 OK\r\nContent-Length: 33554433\r\nConnection: close\r\n\r\n");
    assert!(
        futures::executor::block_on(fetch_remote_bytes(format!("http://127.0.0.1:{p3}/huge")))
            .is_err()
    );
    h3.join().unwrap();
}

//! 用 DPAPI 包裹主密钥(仅 Windows)。
//!
//! `CryptProtectData` 按**当前用户**范围加密(不带 `CRYPTPROTECT_LOCAL_MACHINE`):
//! 密钥文件被拷到别的账户 / 别的机器就解不开。`CRYPTPROTECT_UI_FORBIDDEN` 禁止
//! 任何凭据提示框 —— sidecar 在 hook 冷启动路径上跑,弹一个框就是挂死。
//!
//! 附加熵是固定的应用串:不提供额外保密性,只把这份 blob 与「随便哪个程序用
//! DPAPI 加密的东西」区分开。

use std::ptr;

use windows_sys::Win32::Foundation::{LocalFree, HLOCAL};
use windows_sys::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};

const ENTROPY: &[u8] = b"mini-term credential key v1";

fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        // DPAPI 只读输入 blob;签名里的 `*mut` 是 Win32 头文件的历史遗留。
        pbData: data.as_ptr() as *mut u8,
    }
}

/// DPAPI 分配的输出缓冲:拷出后清零再 `LocalFree`。
struct OutBlob(CRYPT_INTEGER_BLOB);

impl OutBlob {
    fn new() -> Self {
        Self(CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: ptr::null_mut(),
        })
    }

    fn as_slice(&mut self) -> &mut [u8] {
        if self.0.pbData.is_null() {
            return &mut [];
        }
        // SAFETY:pbData / cbData 由成功返回的 DPAPI 调用填写,缓冲在 Drop 前一直有效。
        unsafe { std::slice::from_raw_parts_mut(self.0.pbData, self.0.cbData as usize) }
    }
}

impl Drop for OutBlob {
    fn drop(&mut self) {
        if self.0.pbData.is_null() {
            return;
        }
        self.as_slice().fill(0);
        // SAFETY:DPAPI 文档要求输出缓冲由调用方 LocalFree。
        unsafe {
            LocalFree(self.0.pbData as HLOCAL);
        }
        self.0.pbData = ptr::null_mut();
    }
}

/// 明文 → DPAPI blob。
pub fn protect(data: &[u8]) -> Result<Vec<u8>, String> {
    let input = blob(data);
    let entropy = blob(ENTROPY);
    let mut out = OutBlob::new();
    // SAFETY:全部指针指向本函数栈上有效的结构;输出由 OutBlob 负责释放。
    let ok = unsafe {
        CryptProtectData(
            &input,
            ptr::null(),
            &entropy,
            ptr::null(),
            ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out.0,
        )
    };
    if ok == 0 {
        return Err(format!(
            "CryptProtectData 失败: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(out.as_slice().to_vec())
}

/// DPAPI blob → 明文。换了用户账户 / 机器,或 blob 被改过,这里就失败。
pub fn unprotect(data: &[u8]) -> Result<Vec<u8>, String> {
    let input = blob(data);
    let entropy = blob(ENTROPY);
    let mut out = OutBlob::new();
    // SAFETY:同 `protect`。
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            ptr::null_mut(),
            &entropy,
            ptr::null(),
            ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out.0,
        )
    };
    if ok == 0 {
        return Err(format!(
            "CryptUnprotectData 失败(密钥文件不是当前用户在本机生成的?): {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(out.as_slice().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpapi_round_trips_and_binds_entropy() {
        let secret = b"32-bytes-of-key-material-goes-here!".to_vec();
        let wrapped = protect(&secret).unwrap();
        assert_ne!(wrapped, secret, "blob 不该是明文");
        assert_eq!(unprotect(&wrapped).unwrap(), secret);

        let mut tampered = wrapped.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x55;
        assert!(unprotect(&tampered).is_err(), "改过的 blob 必须解不开");
    }
}

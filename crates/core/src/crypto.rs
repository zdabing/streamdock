use crate::error::{CoreError, CoreResult};
use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

#[derive(Debug, Clone)]
pub struct CryptoKeyInfo {
    pub uri: String,
    pub method: String,
    pub key: [u8; 16],
    pub iv: Option<[u8; 16]>,
}

pub fn parse_iv(iv_hex: Option<&str>, sequence: u64) -> [u8; 16] {
    let mut out = [0u8; 16];
    if let Some(raw) = iv_hex {
        let stripped = raw
            .trim()
            .strip_prefix("0x")
            .or_else(|| raw.trim().strip_prefix("0X"))
            .unwrap_or(raw.trim());
        let padded = if stripped.len() >= 32 {
            stripped[..32].to_string()
        } else {
            format!("{stripped:0>32}")
        };
        if let Ok(bytes) = hex::decode(&padded) {
            if bytes.len() == 16 {
                out.copy_from_slice(&bytes);
                return out;
            }
        }
    }
    out[8..16].copy_from_slice(&sequence.to_be_bytes());
    out
}

pub fn decrypt_segment_data(
    data: &[u8],
    info: Option<&CryptoKeyInfo>,
    override_iv: Option<[u8; 16]>,
) -> CoreResult<Vec<u8>> {
    let Some(crypto) = info else {
        return Ok(data.to_vec());
    };
    if !crypto.method.eq_ignore_ascii_case("AES-128") {
        return Err(CoreError::msg(format!(
            "暂不支持 {} 加密的视频",
            crypto.method
        )));
    }
    let iv = override_iv.or(crypto.iv).unwrap_or([0u8; 16]);
    let mut buf = data.to_vec();
    let decryptor = Aes128CbcDec::new((&crypto.key).into(), (&iv).into());
    let decrypted = decryptor
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|e| CoreError::msg(format!("AES-128 解密失败: {e}")))?;
    Ok(decrypted.to_vec())
}
